//! P6: automatic merger lifecycle (design:
//! docs/superpowers/specs/2026-08-20-p6-merger-lifecycle-design.md).
//!
//! Event-driven, near-zero standing cost: after each completed run, book
//! instruments that have stopped producing observations are asked ONE cheap
//! question (MARKET_STATUS, 1 hit) under a 30-day cooldown. Only a non-ACTV
//! answer spends anything further.
//!
//! The confirmation gate (P0 7.2) is relaxed in exactly one place: a link
//! backed by a Bloomberg CA record -- an Action ID naming target, acquirer,
//! completion date and stock terms -- is ASSERTED by Bloomberg, not inferred
//! by us, and is auto-confirmed as `auto:action:<id>`. Everything less
//! certain (cash-only terms, unresolved acquirer, dead funds, whose M&A
//! table does not exist) stays a proposal plus a durable ingest_issue.

use crate::error::AppResult;
use crate::master_fetch::{ActionTerms, MaDeal, MasterFetcher, MARKET_STATUS_ACTIVE};
use chrono::NaiveDate;
use sqlx::PgPool;

/// No current observation in this many calendar days = worth one question.
/// Lenient enough that an exchange holiday week never triggers it.
pub const STALE_DAYS: i32 = 7;
/// A recorded market_status answer holds this long before re-asking.
pub const RECHECK_DAYS: i32 = 30;
/// Deal actions queried per dead equity before giving up (newest first).
pub const MAX_ACTIONS_QUERIED: usize = 3;
/// The bitemporal attr the verbatim status is recorded under: the 'status'
/// slot migration 0001 reserved for "a lifecycle status P3/P5 may derive"
/// (master_fetch::IdentityBlock.status). P6 is the deriver.
pub const MARKET_STATUS_ATTR: &str = "status";

#[derive(Debug, Default, serde::Serialize)]
pub struct LifecycleSummary {
    /// Instruments whose MARKET_STATUS was asked and recorded.
    pub checked: usize,
    /// Of those, how many answered something other than ACTV.
    pub dead: usize,
    pub links_proposed: usize,
    pub links_confirmed: usize,
    pub issues: usize,
}

/// "1.7234 Aqr sh./Tgt sh." -> 1.7234 (acquirer shares per target share).
///
/// Strict on the unit text: a number followed by anything else is NOT a
/// ratio we understand, and misreading it would corrupt every stitched
/// series through the junction. The decimal separator tolerates a comma --
/// the probe measured a dot even on this French-localized Terminal, but the
/// terminal that localizes CA_MA_PAYMENT_TYP has earned the suspicion.
pub fn parse_stock_terms(s: &str) -> Option<f64> {
    let mut words = s.split_whitespace();
    let ratio: f64 = words.next()?.replace(',', ".").parse().ok()?;
    let unit: Vec<&str> = words.collect();
    let unit = unit.join(" ");
    (ratio > 0.0 && unit.contains("Aqr") && unit.contains("Tgt")).then_some(ratio)
}

/// Active book instruments in active views with no current observation
/// within STALE_DAYS, holding a security valid today, and without a
/// market_status answer recorded within RECHECK_DAYS. Empty on a healthy
/// book -- which is what makes the post-run hook free on ordinary days.
///
/// Deliberately NOT gated on "has ever produced an observation": a security
/// that was already dead when it was added to the book (live case: YODA LN,
/// acquired 2024, added 2026) has zero observations and would burn its
/// request hits daily forever while staying invisible to that gate.
pub async fn stale_candidates(pool: &PgPool, today: NaiveDate)
    -> AppResult<Vec<(i64, String)>>
{
    Ok(sqlx::query_as(
        "SELECT DISTINCT ON (be.instrument_id) be.instrument_id, a.value
           FROM book_entry be
           JOIN instrument_alias a ON a.instrument_id = be.instrument_id
                AND a.id_type = 'bdp_security'
                AND a.valid_from <= $1 AND a.valid_to > $1
                AND a.system_to = 'infinity'
          WHERE be.active
            AND EXISTS (SELECT 1 FROM view_instrument vi
                          JOIN view v ON v.id = vi.view_id AND v.active
                         WHERE vi.instrument_id = be.instrument_id)
            AND NOT EXISTS (SELECT 1 FROM observation o
                             WHERE o.instrument_id = be.instrument_id
                               AND o.system_to = 'infinity'
                               AND o.obs_date >= $1::date - $2)
            AND NOT EXISTS (SELECT 1 FROM instrument_attr ia
                             WHERE ia.instrument_id = be.instrument_id
                               AND ia.attr = $4
                               AND ia.system_to = 'infinity'
                               AND ia.valid_from > $1::date - $3)
          ORDER BY be.instrument_id, a.valid_from DESC")
        .bind(today).bind(STALE_DAYS).bind(RECHECK_DAYS).bind(MARKET_STATUS_ATTR)
        .fetch_all(pool).await?)
}

/// The whole check: candidates -> batched MARKET_STATUS -> per-dead-name
/// investigation. Per-instrument failures are isolated (stderr) so one
/// name's bad day cannot hide another's merger, matching refresh_view.
pub async fn run_check<F: MasterFetcher>(pool: &PgPool, fetcher: &F,
                                         today: NaiveDate)
    -> AppResult<LifecycleSummary>
{
    let mut summary = LifecycleSummary::default();
    let candidates = stale_candidates(pool, today).await?;
    if candidates.is_empty() {
        return Ok(summary);
    }
    for chunk in candidates.chunks(crate::fetch::MAX_SECURITIES_PER_REQUEST) {
        let securities: Vec<String> = chunk.iter().map(|(_, s)| s.clone()).collect();
        let answered = fetcher.market_status(&securities).await?;
        let by_security: std::collections::HashMap<&str, &str> = answered.parsed
            .iter().map(|(s, st)| (s.as_str(), st.as_str())).collect();
        for (instrument_id, security) in chunk {
            // Silence is "unknown", never "active": no attr is written, so
            // the cooldown does not arm and tomorrow asks again.
            let Some(status) = by_security.get(security.as_str()) else { continue };
            record_status(pool, *instrument_id, status, today).await?;
            summary.checked += 1;
            if *status == MARKET_STATUS_ACTIVE {
                continue;
            }
            summary.dead += 1;
            if let Err(e) = investigate(pool, fetcher, *instrument_id, security,
                                        status, today, &mut summary).await {
                eprintln!("lifecycle: investigating {security} failed: {e}");
            }
        }
    }
    Ok(summary)
}

/// The verbatim status becomes a bitemporal attr, like any other identity
/// fact: what Bloomberg said, when it said it, corrected by close-and-insert
/// if it ever changes.
async fn record_status(pool: &PgPool, instrument_id: i64, status: &str,
                       today: NaiveDate) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    // source 'bloomberg': the value is MARKET_STATUS verbatim, not a local
    // derivation -- and the only three sources the schema admits are
    // bloomberg/user/derived.
    crate::instrument::store::set_attr(
        &mut tx, instrument_id, MARKET_STATUS_ATTR, status, today,
        "bloomberg", None).await?;
    tx.commit().await?;
    Ok(())
}

async fn investigate<F: MasterFetcher>(pool: &PgPool, fetcher: &F,
                                       instrument_id: i64, security: &str,
                                       status: &str, today: NaiveDate,
                                       summary: &mut LifecycleSummary)
    -> AppResult<()>
{
    // P9: a class can opt out of M&A investigation entirely (asset_class
    // .ma_capable, migration 0011) -- the called-bond story (design 9.3).
    // book_entry.instrument_id is the primary key, so at most one row; an
    // instrument with no book_entry at all keeps today's behaviour, which is
    // what unwrap_or(true) encodes.
    let ma_capable: bool = sqlx::query_scalar(
        "SELECT ac.ma_capable FROM book_entry be
         JOIN asset_class ac ON ac.id = be.asset_class_id
         WHERE be.instrument_id = $1")
        .bind(instrument_id).fetch_optional(pool).await?
        .unwrap_or(true);
    if !ma_capable {
        return retire_path(pool, fetcher, instrument_id, security, today, summary).await;
    }

    let deals = fetcher.ma_deals(security).await?;
    if deals.parsed.not_applicable {
        return fund_path(pool, fetcher, instrument_id, security, status,
                         today, summary).await;
    }

    // Equity route. The deal list also contains deals where THIS instrument
    // was the acquirer (measured: XLNX's list holds its own acquisitions),
    // so the discriminator is CA_MA_TARGET_TICKER matching us -- recency
    // only orders the candidates, it never decides.
    let own_ticker = security.strip_suffix(" Equity").unwrap_or(security);
    let mut candidates: Vec<&MaDeal> = deals.parsed.deals.iter()
        .filter(|d| d.deal_status.as_deref() == Some("Completed")
            && d.deal_type.as_deref() == Some("M&A"))
        .collect();
    candidates.sort_by_key(|d| std::cmp::Reverse(d.announce_date));

    let mut queried = 0usize;
    for deal in candidates.into_iter().take(MAX_ACTIONS_QUERIED) {
        queried += 1;
        let answered = fetcher.action_terms(&deal.action_id).await?;
        let Some(terms) = answered.parsed else { continue };
        if terms.target_ticker.as_deref() != Some(own_ticker) {
            continue;
        }
        return build_link(pool, instrument_id, deal, &terms, today, summary).await;
    }
    record_issue(pool, instrument_id, "lifecycle_no_deal_match",
        &format!("{security} answered MARKET_STATUS {status} but none of the \
                  {queried} newest completed M&A actions name it as target; \
                  find the successor manually (CACS <GO>) and link it in Review"),
        summary).await
}

async fn build_link(pool: &PgPool, instrument_id: i64, deal: &MaDeal,
                    terms: &ActionTerms, _today: NaiveDate,
                    summary: &mut LifecycleSummary) -> AppResult<()>
{
    let Some(effective) = terms.complete_dt else {
        return record_issue(pool, instrument_id, "merger_terms_incomplete",
            &format!("action {}: names this instrument as target but \
                      CA_MA_COMPLETE_DT is empty; no link without an \
                      effective date", terms.action_id), summary).await;
    };
    let Some(acquirer_ticker) = terms.acquirer_ticker.as_deref() else {
        return record_issue(pool, instrument_id, "merger_acquirer_unresolved",
            &format!("action {}: no CA_MA_ACQUIRER_TICKER; link the successor \
                      manually in Review", terms.action_id), summary).await;
    };
    // "AMD US" -> "AMD US Equity", the bdp_security space aliases live in.
    let acquirer_security = format!("{acquirer_ticker} Equity");
    let owners: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT instrument_id FROM instrument_alias
          WHERE id_type = 'bdp_security' AND value = $1
            AND valid_from <= $2 AND valid_to > $2 AND system_to = 'infinity'")
        .bind(&acquirer_security).bind(effective)
        .fetch_all(pool).await?;
    let [successor_id] = owners[..] else {
        return record_issue(pool, instrument_id, "merger_acquirer_unresolved",
            &format!("action {}: acquirer {acquirer_security} maps to {} local \
                      instruments as of {effective}; add or disambiguate it, \
                      then re-run", terms.action_id, owners.len()), summary).await;
    };
    if successor_id == instrument_id {
        return record_issue(pool, instrument_id, "merger_acquirer_unresolved",
            &format!("action {}: acquirer {acquirer_security} resolves to the \
                      target itself; refusing a self-link", terms.action_id),
            summary).await;
    }

    let ratio = terms.stock_terms.as_deref().and_then(parse_stock_terms);
    let evidence = serde_json::json!({
        "source": "lifecycle",
        "action_id": terms.action_id,
        "deal_row": deal.row,
        "terms": terms.raw,
    });
    // Same duplicate guard as history's proposals: the exact tuple already
    // on file (confirmed or not) is reused, never duplicated.
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM instrument_link
          WHERE predecessor_id = $1 AND successor_id = $2
            AND link_type = 'merger' AND effective_date = $3")
        .bind(instrument_id).bind(successor_id).bind(effective)
        .fetch_optional(pool).await?;
    let link_id = match existing {
        Some(id) => id,
        None => {
            let id = crate::instrument::store::propose_link(
                pool, instrument_id, successor_id, "merger", effective,
                evidence).await?;
            summary.links_proposed += 1;
            id
        }
    };
    crate::instrument::store::set_link_terms(
        pool, link_id, ratio, Some(terms.raw.clone())).await?;

    if ratio.is_some() {
        // Asserted by Bloomberg (Action ID + parsed stock terms): the one
        // case the design auto-confirms. confirm_link is a no-op on an
        // already-confirmed row, so re-running never rewrites who confirmed.
        crate::instrument::store::confirm_link(
            pool, link_id, &format!("auto:action:{}", terms.action_id)).await?;
        summary.links_confirmed += 1;
    } else {
        record_issue(pool, instrument_id, "merger_cash_terms",
            &format!("action {}: no parsable stock terms ({:?}); a cash-out \
                      has no series to continue, so the link waits in Review",
                     terms.action_id, terms.stock_terms), summary).await?;
    }
    Ok(())
}

/// The 6-year `HISTORICAL_IDS_TIME_RANGE` ingest both dead-instrument routes
/// need: it is how a delisting's Action IDs and any recorded rename chain
/// reach `instrument_alias`/`instrument_link` on their own, via the existing
/// history machinery (`instrument::history`). Shared so `fund_path` and
/// `retire_path` cannot drift on how that refresh is invoked.
async fn refresh_identity_history<F: MasterFetcher>(pool: &PgPool, fetcher: &F,
                                                     instrument_id: i64, security: &str,
                                                     today: NaiveDate)
    -> AppResult<crate::instrument::history::HistoryOutcome>
{
    let start = today - chrono::Days::new(6 * 365);
    crate::instrument::history::ingest(pool, fetcher, instrument_id, security, start).await
}

/// Funds: the M&A table does not exist and no field names a successor
/// (probed exhaustively, design 2). What the API does give is identifier
/// history -- Action IDs of the delisting, and a rename chain when one was
/// recorded, which the existing history machinery turns into aliases and
/// proposals on its own. The durable issue tells the user the one thing
/// automation cannot do: pick the absorbing fund.
async fn fund_path<F: MasterFetcher>(pool: &PgPool, fetcher: &F,
                                     instrument_id: i64, security: &str,
                                     status: &str, today: NaiveDate,
                                     summary: &mut LifecycleSummary)
    -> AppResult<()>
{
    match refresh_identity_history(pool, fetcher, instrument_id, security, today).await {
        Ok(outcome) => {
            summary.links_proposed += outcome.links_proposed.len();
            record_issue(pool, instrument_id, "lifecycle_dead_fund",
                &format!("{security} answered MARKET_STATUS {status}; funds \
                          expose no M&A table and no successor field, so the \
                          absorbing fund must be picked by hand (CACX <GO>). \
                          Identifier history ingested: {} aliases, {} link \
                          proposals. Once a successor link is confirmed, the \
                          stitched series derives the ratio from NAV \
                          continuity at the junction -- for funds that IS the \
                          official mechanism. Consider retiring this \
                          instrument to stop spending daily hits on it.",
                         outcome.aliases_added, outcome.links_proposed.len()),
                summary).await
        }
        Err(e) => record_issue(pool, instrument_id, "lifecycle_dead_fund",
            &format!("{security} answered MARKET_STATUS {status}; identifier \
                      history fetch failed: {e}"), summary).await,
    }
}

/// A dead instrument whose class opted out of M&A investigation entirely
/// (`asset_class.ma_capable = FALSE`) -- the called-bond story (design 9.3):
/// a called or matured bond gets `INACTIVE_DATE` from Bloomberg and ends its
/// series exactly like a delisted equity, with no successor to look for.
///
/// Gets the same identifier-history refresh `fund_path` performs -- so a
/// delisting Action ID still lands if Bloomberg records one -- but proposes
/// no link and burns no `ma_deals` hit; the class has already said M&A does
/// not apply. The refresh is advisory here, same as everywhere else in this
/// module: a failure is logged and the retirement issue is still recorded,
/// never lost behind a propagated error.
async fn retire_path<F: MasterFetcher>(pool: &PgPool, fetcher: &F,
                                       instrument_id: i64, security: &str,
                                       today: NaiveDate,
                                       summary: &mut LifecycleSummary)
    -> AppResult<()>
{
    if let Err(e) = refresh_identity_history(pool, fetcher, instrument_id, security, today).await {
        eprintln!("lifecycle: retire_path identity refresh for {security} failed: {e}");
    }
    record_issue(pool, instrument_id, "lifecycle_retired",
        "instrument inactive; class opted out of M&A investigation -- series \
         capped at INACTIVE_DATE, retire the book entry", summary).await
}

/// Durable, queryable, idempotent on (instrument_id, code, detail) -- the
/// same natural key the history module uses. stderr reaches nobody in a
/// Tauri binary.
async fn record_issue(pool: &PgPool, instrument_id: i64, code: &str,
                      detail: &str, summary: &mut LifecycleSummary)
    -> AppResult<()>
{
    let inserted = sqlx::query(
        "INSERT INTO ingest_issue (run_id, instrument_id, severity, code, detail)
         SELECT NULL, $1, 'warn', $2, $3
          WHERE NOT EXISTS (SELECT 1 FROM ingest_issue
                             WHERE instrument_id = $1 AND code = $2 AND detail = $3)")
        .bind(instrument_id).bind(code).bind(detail)
        .execute(pool).await?;
    if inserted.rows_affected() > 0 {
        summary.issues += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both live-measured shapes, plus the refusals that guard the splice.
    #[test]
    fn stock_terms_parse_the_measured_formats() {
        assert_eq!(parse_stock_terms("1.7234 Aqr sh./Tgt sh."), Some(1.7234));
        assert_eq!(parse_stock_terms("1 Aqr sh./Tgt sh."), Some(1.0));
        assert_eq!(parse_stock_terms("1,7234 Aqr sh./Tgt sh."), Some(1.7234),
                   "a comma decimal on a localized Terminal still parses");
    }

    #[test]
    fn unrecognised_terms_are_refused_not_guessed() {
        assert_eq!(parse_stock_terms("50/sh."), None, "cash terms are not a ratio");
        assert_eq!(parse_stock_terms("1.7234"), None, "a bare number proves nothing");
        assert_eq!(parse_stock_terms("Aqr sh./Tgt sh."), None);
        assert_eq!(parse_stock_terms("0 Aqr sh./Tgt sh."), None,
                   "a zero ratio would divide the world by zero downstream");
        assert_eq!(parse_stock_terms("-2 Aqr sh./Tgt sh."), None);
        assert_eq!(parse_stock_terms(""), None);
    }
}
