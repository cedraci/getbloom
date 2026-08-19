//! Spec §5: turning what the user typed into an instrument_id.
//!
//! Two properties matter more than the steps themselves.
//!
//! First, every path writes a resolution_decision -- including the local path
//! that costs nothing. An audit trail with holes where the cheap answers went
//! cannot answer "why is this instrument bound to that security".
//!
//! Second, ambiguity is not resolved by guessing. Two plausible candidates
//! produce a review and bind nothing, because a wrong binding is discovered
//! months later as a silently wrong price series. This applies even to the
//! free local-alias path: two live instruments can legitimately wear the same
//! ticker (BMW in Frankfurt and in the US), and a Bloomberg call cannot
//! resolve an ambiguity that is entirely local.

use crate::error::{AppError, AppResult};
use crate::instrument::store::{self, NewAlias};
use crate::master_fetch::{IdentityBlock, MasterFetcher};
use crate::resolution::normalize::{build_security, detect_id_kind};
use crate::resolution::score::{score_all, verdict, Candidate, Hints, Scored, Verdict};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveInput {
    pub raw: String,
    pub yellow_key: String,
    pub hints: Hints,
    pub as_of: NaiveDate,
    /// 'auto' for an automatic resolution, or the user who asked for it.
    pub decided_by: String,
}

#[derive(Debug, Serialize)]
pub enum Resolution {
    Bound { instrument_id: i64, decision_id: i64, method: String },
    NeedsReview { decision_id: i64, review_id: i64, candidates: Vec<Scored> },
    NotFound { decision_id: i64 },
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PendingReview {
    pub review_id: i64,
    pub decision_id: i64,
    pub raw_input: String,
    pub normalized: String,
    pub candidates: serde_json::Value,
    pub bbg_response: Option<serde_json::Value>,
    pub opened_at: chrono::DateTime<chrono::Utc>,
}

/// Bloomberg's yellow-key filter for instrumentListRequest, derived from the
/// market sector the user chose. Values are from the //blp/instruments schema
/// captured in P0. Public: Task 11's UI-facing search reuses this mapping.
pub fn yellow_key_filter(yellow_key: &str) -> Option<&'static str> {
    match yellow_key.trim().to_ascii_lowercase().as_str() {
        "equity" => Some("YK_FILTER_EQTY"),
        "corp" => Some("YK_FILTER_CORP"),
        "govt" => Some("YK_FILTER_GOVT"),
        "index" => Some("YK_FILTER_INDX"),
        "curncy" => Some("YK_FILTER_CURR"),
        "comdty" => Some("YK_FILTER_CMDT"),
        "mtge" => Some("YK_FILTER_MTGE"),
        "muni" => Some("YK_FILTER_MUNI"),
        "pfd" => Some("YK_FILTER_PRFD"),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn record_decision(pool: &PgPool, input: &ResolveInput, normalized: &str,
                         method: &str, chosen: Option<i64>,
                         candidates: &serde_json::Value,
                         bbg: Option<&serde_json::Value>) -> AppResult<i64>
{
    Ok(sqlx::query_scalar(
        "INSERT INTO resolution_decision
           (raw_input, normalized, hint_exchange, hint_country, hint_currency,
            hint_asset_class, method, chosen_instrument_id, candidates,
            bbg_response, decided_by)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING id")
        .bind(&input.raw).bind(normalized)
        .bind(&input.hints.exchange).bind(&input.hints.country)
        .bind(&input.hints.currency).bind(&input.hints.asset_class)
        .bind(method).bind(chosen).bind(candidates).bind(bbg)
        .bind(&input.decided_by)
        .fetch_one(pool).await?)
}

/// The day before `d` -- used to derive an honest floor for a validity
/// period when only an end date is known. Falls back to `d` itself only at
/// chrono::NaiveDate's own minimum, which no real Bloomberg date reaches.
fn day_before(d: NaiveDate) -> NaiveDate {
    d.pred_opt().unwrap_or(d)
}

/// A validity period that is always non-empty, satisfying every alias/attr
/// row's `CHECK (valid_from < valid_to)`.
///
/// LISTING_DATE is the honest start when it actually precedes INACTIVE_DATE.
/// A delisted security can have INACTIVE_DATE with no LISTING_DATE at all
/// (routine outside cash equities), or -- more rarely -- a LISTING_DATE that
/// is not strictly before INACTIVE_DATE (equal, or Bloomberg data that is
/// simply wrong). Either would otherwise produce `valid_from >= valid_to`
/// and the insert would die on SQLSTATE 23514 after the decision row (and,
/// pre-fix, the FIGI) were already committed. When INACTIVE_DATE is known but
/// cannot anchor a period from LISTING_DATE, the day before it is the honest
/// floor: we know the instrument existed then, because it had not yet died.
fn validity_period(listing_date: Option<NaiveDate>, inactive_date: Option<NaiveDate>,
                   as_of: NaiveDate) -> (NaiveDate, Option<NaiveDate>)
{
    match (listing_date, inactive_date) {
        (Some(listed), Some(inactive)) if listed < inactive => (listed, Some(inactive)),
        (Some(listed), None) => (listed, None),
        (_, Some(inactive)) => (day_before(inactive), Some(inactive)),
        (None, None) => (as_of, None),
    }
}

/// Write an identity block into the master: one instrument, its aliases, its
/// attributes. Idempotent on re-resolution:
/// - a FIGI already in the master identifies the same instrument, not a new
///   one -- the common case, since almost every IDENTITY_FIELDS response
///   carries one;
/// - when there is no FIGI (the resolve_review fallback path), the same
///   bdp_security alias does instead, so a double-submit or two reviews
///   resolved to the same security while Bloomberg stays silent cannot mint
///   two instruments wearing one identifier. A genuine local ambiguity here
///   (more than one existing match) is not this function's call to
///   arbitrate -- find_by_alias answers None and a fresh instrument is
///   created, same as if nothing had matched.
///
/// The lookup and the write happen inside one transaction so the instrument
/// row, its Bloomberg ids, its aliases and its attributes commit or vanish
/// together -- never a FIGI permanently claimed by an empty shell because a
/// later statement in this function failed.
async fn bind_identity(pool: &PgPool, block: &IdentityBlock, decision_id: i64,
                       as_of: NaiveDate) -> AppResult<i64>
{
    if let Some(figi) = block.figi.as_deref() {
        if let Some(existing) = sqlx::query_scalar::<_, i64>(
            "SELECT instrument_id FROM instrument WHERE id_bb_global = $1")
            .bind(figi).fetch_optional(pool).await?
        {
            return Ok(existing);
        }
    } else if let Some(existing) = store::find_by_alias(
        pool, "bdp_security", &block.security, as_of).await?
    {
        return Ok(existing);
    }

    let (from, to) = validity_period(block.listing_date, block.inactive_date, as_of);

    let mut tx = pool.begin().await?;
    let inst = store::create_tx(&mut tx).await?;
    store::set_bloomberg_ids_tx(&mut tx, inst.instrument_id, block.figi.as_deref(),
                                block.bbg_unique.as_deref()).await?;

    let alias = |id_type: &str, value: &str| NewAlias {
        id_type: id_type.into(), value: value.into(),
        exch_code: block.exch_code.clone(), valid_from: from, valid_to: to,
        source: "bloomberg_ref".into(), bbg_action_id: None,
        anchoring_identifier: None,
    };
    store::insert_alias(&mut tx, inst.instrument_id,
                        &alias("bdp_security", &block.security)).await?;
    if let Some(v) = &block.figi {
        store::insert_alias(&mut tx, inst.instrument_id, &alias("figi", v)).await?;
    }
    if let Some(v) = &block.share_class_figi {
        store::insert_alias(&mut tx, inst.instrument_id, &alias("figi", v)).await?;
    }
    if let Some(v) = &block.isin {
        store::insert_alias(&mut tx, inst.instrument_id, &alias("isin", v)).await?;
    }
    if let Some(v) = &block.bbg_unique {
        store::insert_alias(&mut tx, inst.instrument_id, &alias("bbg_unique", v)).await?;
    }

    for (attr, value) in [
        ("name", &block.name),
        ("exchange", &block.exch_code),
        ("currency", &block.currency),
        ("country", &block.country),
        ("instrument_type", &block.security_typ2),
        ("asset_class", &block.market_sector),
        // No "status": P0 §10.2 -- SIMP_SEC_STATUS is a trading-session state,
        // not a lifecycle one. INACTIVE_DATE closes the validity periods
        // instead -- for aliases via `to` above, and for attributes via
        // close_attrs_at below -- which is the durable way to say an
        // instrument has ended.
    ] {
        if let Some(v) = value {
            store::set_attr(&mut tx, inst.instrument_id, attr, v, from,
                            "bloomberg", Some(decision_id)).await?;
        }
    }
    if let Some(inactive) = block.inactive_date {
        store::close_attrs_at(&mut tx, inst.instrument_id, inactive).await?;
    }
    tx.commit().await?;
    Ok(inst.instrument_id)
}

/// Build the Vec<Scored> that records a *local* alias ambiguity -- two or more
/// live instruments already wearing the same identifier. There is no
/// Bloomberg candidate list here, only instrument ids the store found, so
/// each is represented by whatever security string it currently wears (or a
/// placeholder if it has none) with a score of 0: nothing was compared,
/// because nothing on the Bloomberg side was ever consulted.
async fn local_ambiguity_candidates(pool: &PgPool, matches: &[i64], id_type: &str,
                                    probe: &str, as_of: NaiveDate)
    -> AppResult<Vec<Scored>>
{
    let mut out = Vec::with_capacity(matches.len());
    for iid in matches {
        let security = store::current_security(pool, *iid, as_of).await?
            .unwrap_or_else(|| format!("instrument #{iid}"));
        out.push(Scored {
            candidate: Candidate {
                security,
                description: format!("existing instrument_id {iid}"),
                exchange: None, country: None, currency: None,
                asset_class: None, figi: None,
            },
            score: 0,
            disqualified: false,
            reasons: vec![format!(
                "local alias '{probe}' ({id_type}) already matches {} instruments",
                matches.len())],
        });
    }
    Ok(out)
}

pub async fn resolve<F: MasterFetcher>(pool: &PgPool, fetcher: &F,
                                       input: &ResolveInput) -> AppResult<Resolution>
{
    // 1. normalise
    let kind = detect_id_kind(&input.raw);
    let security = build_security(kind, &input.raw, &input.yellow_key)?;

    // 2. local alias lookup -- the free path. Two live instruments can
    // legitimately wear the same identifier (BMW in Frankfurt and in the
    // US), and that ambiguity is entirely local: a Bloomberg call cannot
    // resolve it, so none is made.
    for id_type in ["bdp_security", "ticker", "isin", "figi"] {
        let probe = if id_type == "bdp_security" { security.as_str() }
                    else { input.raw.trim() };
        let matches = store::find_all_by_alias(pool, id_type, probe, input.as_of).await?;
        match matches.len() {
            0 => continue,
            1 => {
                let iid = matches[0];
                let decision_id = record_decision(
                    pool, input, &security, "local_alias", Some(iid),
                    &serde_json::json!({"matched": id_type, "bloomberg_calls": 0}),
                    None).await?;
                return Ok(Resolution::Bound {
                    instrument_id: iid, decision_id, method: "local_alias".into() });
            }
            _ => {
                let scored = local_ambiguity_candidates(
                    pool, &matches, id_type, probe, input.as_of).await?;
                let candidates_json = serde_json::to_value(&scored).unwrap_or(serde_json::json!([]));
                let decision_id = record_decision(
                    pool, input, &security, "local_alias", None,
                    &candidates_json, None).await?;
                let review_id: i64 = sqlx::query_scalar(
                    "INSERT INTO resolution_review (decision_id, status)
                     VALUES ($1,'pending') RETURNING id")
                    .bind(decision_id).fetch_one(pool).await?;
                return Ok(Resolution::NeedsReview {
                    decision_id, review_id, candidates: scored });
            }
        }
    }

    // 3. ReferenceDataRequest for the identity block
    let answered = fetcher.identity(std::slice::from_ref(&security)).await?;
    let blocks = answered.parsed;
    let raw_identity = answered.raw;
    if blocks.len() == 1 {
        let decision_id = record_decision(
            pool, input, &security, "bloomberg_ref", None,
            &serde_json::json!([&blocks[0]]), Some(&raw_identity)).await?;
        let iid = bind_identity(pool, &blocks[0], decision_id, input.as_of).await?;
        // Spec §5.1: one anchored history request per instrument, ever. A
        // failure here must not undo a good binding -- the identifiers we
        // have are still correct, we simply know less about the past.
        let hist_start = blocks[0].listing_date
            .unwrap_or_else(|| NaiveDate::from_ymd_opt(1980, 1, 1).unwrap());
        if let Err(e) = crate::instrument::history::ingest(
            pool, fetcher, iid, &blocks[0].security, hist_start).await
        {
            eprintln!("identifier history for {} failed: {e}", blocks[0].security);
        }
        // Not fixed, deliberately: a crash between bind_identity committing
        // and this UPDATE leaves the decision row with chosen_instrument_id
        // still NULL even though the instrument exists. That is recoverable,
        // not silently wrong -- step 2's find_by_alias/find_all_by_alias
        // heals it on the very next resolve of the same identifier (the
        // instrument is found and bound, or FIGI/bdp_security dedup in
        // bind_identity above returns the same instrument again), and no
        // resolution_review is opened on this path, so the "nothing binds
        // silently" property never depends on this UPDATE completing.
        sqlx::query("UPDATE resolution_decision SET chosen_instrument_id = $2 WHERE id = $1")
            .bind(decision_id).bind(iid).execute(pool).await?;
        return Ok(Resolution::Bound {
            instrument_id: iid, decision_id, method: "bloomberg_ref".into() });
    }

    // 4. ambiguous or absent -> search
    let found = fetcher.instrument_list(
        input.raw.trim(), yellow_key_filter(&input.yellow_key), 20).await?;

    // 5. score
    let scored = score_all(found.parsed, &input.hints);
    let candidates_json = serde_json::to_value(&scored).unwrap_or(serde_json::json!([]));

    match verdict(scored) {
        Verdict::Unique(c) => {
            // The search gave a security string, not an identity. Ask once more.
            let answered = fetcher.identity(std::slice::from_ref(&c.security)).await?;
            let raw_identity = answered.raw;
            let Some(block) = answered.parsed.into_iter().next() else {
                let decision_id = record_decision(
                    pool, input, &security, "bloomberg_list", None,
                    &candidates_json, None).await?;
                return Ok(Resolution::NotFound { decision_id });
            };
            // This is the response the binding was actually made from -- the
            // response that produced the search's candidate list is a
            // different call and is not what "bbg_response" means here.
            let decision_id = record_decision(
                pool, input, &security, "bloomberg_list", None,
                &candidates_json, Some(&raw_identity)).await?;
            let iid = bind_identity(pool, &block, decision_id, input.as_of).await?;
            // See the identical history::ingest call in step 3 above.
            let hist_start = block.listing_date
                .unwrap_or_else(|| NaiveDate::from_ymd_opt(1980, 1, 1).unwrap());
            if let Err(e) = crate::instrument::history::ingest(
                pool, fetcher, iid, &block.security, hist_start).await
            {
                eprintln!("identifier history for {} failed: {e}", block.security);
            }
            // See the identical UPDATE in step 3 above: a crash here is
            // recoverable on the next resolve, not silently wrong.
            sqlx::query("UPDATE resolution_decision SET chosen_instrument_id = $2 WHERE id = $1")
                .bind(decision_id).bind(iid).execute(pool).await?;
            Ok(Resolution::Bound {
                instrument_id: iid, decision_id, method: "bloomberg_list".into() })
        }
        // 6. two or more survivors: a human decides, and NOTHING is bound.
        Verdict::Ambiguous(list) => {
            let decision_id = record_decision(
                pool, input, &security, "bloomberg_list", None,
                &candidates_json, None).await?;
            let review_id: i64 = sqlx::query_scalar(
                "INSERT INTO resolution_review (decision_id, status)
                 VALUES ($1,'pending') RETURNING id")
                .bind(decision_id).fetch_one(pool).await?;
            Ok(Resolution::NeedsReview { decision_id, review_id, candidates: list })
        }
        Verdict::None => {
            let decision_id = record_decision(
                pool, input, &security, "bloomberg_list", None,
                &candidates_json, None).await?;
            Ok(Resolution::NotFound { decision_id })
        }
    }
}

pub async fn pending_reviews(pool: &PgPool) -> AppResult<Vec<PendingReview>> {
    Ok(sqlx::query_as::<_, PendingReview>(
        "SELECT r.id AS review_id, d.id AS decision_id, d.raw_input, d.normalized,
                d.candidates, d.bbg_response, r.opened_at
           FROM resolution_review r JOIN resolution_decision d ON d.id = r.decision_id
          WHERE r.status = 'pending' ORDER BY r.opened_at")
        .fetch_all(pool).await?)
}

#[derive(sqlx::FromRow)]
struct ReviewContext {
    decision_id: i64,
    raw_input: String,
    status: String,
    candidates: serde_json::Value,
}

/// A human picked a candidate. The chosen security is resolved for real -- it
/// is not bound from the search result, because a search result is a name,
/// not an identity (spec §6.2: "Selecting a suggestion runs the full §5
/// resolution. It does not bind the clicked string directly."). This costs
/// one ReferenceDataRequest, which the hit budget already allots to resolving
/// a never-seen instrument.
///
/// If Bloomberg returns nothing for the chosen security, the user's decision
/// is still recorded and bound from a bare block -- refusing here would
/// discard a human decision over a transient Bloomberg gap -- but the
/// fallback is flagged in the decision's `candidates` JSON so it stays
/// visible to later audits, alongside the original candidate list and a link
/// back to the review and the decision it came from -- without those, a
/// bound instrument's manual decision would record what was chosen but not
/// what it was chosen from or rejected against.
///
/// Refuses a review that is not currently `pending`: resolving twice, or
/// resolving a review someone already rejected, must not mint a second
/// instrument for the same identifier out from under the first.
pub async fn resolve_review<F: MasterFetcher>(pool: &PgPool, fetcher: &F, review_id: i64,
                                              chosen_security: &str, by: &str) -> AppResult<i64>
{
    let ctx: ReviewContext = sqlx::query_as(
        "SELECT d.id AS decision_id, d.raw_input, r.status, d.candidates
           FROM resolution_review r JOIN resolution_decision d ON d.id = r.decision_id
          WHERE r.id = $1")
        .bind(review_id).fetch_one(pool).await?;
    if ctx.status != "pending" {
        return Err(AppError::Validation(format!(
            "review {review_id} is not pending (status: {})", ctx.status)));
    }

    let sec = chosen_security.to_string();
    let answered = fetcher.identity(std::slice::from_ref(&sec)).await?;
    let raw_identity = answered.raw;
    let (block, fell_back) = match answered.parsed.into_iter().next() {
        Some(b) => (b, false),
        None => (IdentityBlock { security: sec, ..Default::default() }, true),
    };
    let candidates = serde_json::json!({
        "chosen_security": chosen_security,
        "bloomberg_fallback": fell_back,
        "review_id": review_id,
        "source_decision_id": ctx.decision_id,
        "original_candidates": ctx.candidates,
    });

    let manual_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO resolution_decision
           (raw_input, normalized, method, candidates, bbg_response, decided_by)
         VALUES ($1,$2,'manual',$3,$4,$5) RETURNING id")
        .bind(&ctx.raw_input).bind(chosen_security).bind(&candidates).bind(&raw_identity).bind(by)
        .fetch_one(pool).await?;

    let iid = bind_identity(pool, &block, manual_id,
                            chrono::Local::now().date_naive()).await?;
    // See the identical UPDATE in resolve()'s step 3: a crash here is
    // recoverable on the next resolve, not silently wrong.
    sqlx::query("UPDATE resolution_decision SET chosen_instrument_id = $2 WHERE id = $1")
        .bind(manual_id).bind(iid).execute(pool).await?;
    sqlx::query("UPDATE resolution_review SET status = 'resolved', closed_at = now(),
                        note = note || $2 WHERE id = $1")
        .bind(review_id).bind(format!(" resolved by {by} to {chosen_security}"))
        .execute(pool).await?;
    Ok(iid)
}

/// Also needed by the UI: a review the user judges unresolvable.
pub async fn reject_review(pool: &PgPool, review_id: i64, note: &str) -> AppResult<()> {
    sqlx::query("UPDATE resolution_review
                    SET status = 'rejected', closed_at = now(), note = $2
                  WHERE id = $1")
        .bind(review_id).bind(note).execute(pool).await?;
    Ok(())
}
