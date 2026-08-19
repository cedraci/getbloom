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

use crate::error::AppResult;
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

/// Write an identity block into the master: one instrument, its aliases, its
/// attributes. Idempotent on re-resolution because find_by_alias runs first.
async fn bind_identity(pool: &PgPool, block: &IdentityBlock, decision_id: i64,
                       as_of: NaiveDate) -> AppResult<i64>
{
    // A FIGI already in the master is the same instrument, not a new one.
    if let Some(figi) = block.figi.as_deref() {
        if let Some(existing) = sqlx::query_scalar::<_, i64>(
            "SELECT instrument_id FROM instrument WHERE id_bb_global = $1")
            .bind(figi).fetch_optional(pool).await?
        {
            return Ok(existing);
        }
    }
    let inst = store::create(pool).await?;
    store::set_bloomberg_ids(pool, inst.instrument_id, block.figi.as_deref(),
                             block.bbg_unique.as_deref()).await?;

    // Listing date is the honest start of every identifier's validity; without
    // one, today is the only date we can defend.
    let from = block.listing_date.unwrap_or(as_of);
    let to = block.inactive_date;

    let mut tx = pool.begin().await?;
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
        // not a lifecycle one. INACTIVE_DATE above already closes the validity
        // periods, which is the durable way to say an instrument has ended.
    ] {
        if let Some(v) = value {
            store::set_attr(&mut tx, inst.instrument_id, attr, v, from,
                            "bloomberg", Some(decision_id)).await?;
        }
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
/// visible to later audits.
pub async fn resolve_review<F: MasterFetcher>(pool: &PgPool, fetcher: &F, review_id: i64,
                                              chosen_security: &str, by: &str) -> AppResult<i64>
{
    let raw_input: String = sqlx::query_scalar(
        "SELECT d.raw_input FROM resolution_review r
           JOIN resolution_decision d ON d.id = r.decision_id WHERE r.id = $1")
        .bind(review_id).fetch_one(pool).await?;

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
    });

    let manual_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO resolution_decision
           (raw_input, normalized, method, candidates, bbg_response, decided_by)
         VALUES ($1,$2,'manual',$3,$4,$5) RETURNING id")
        .bind(&raw_input).bind(chosen_security).bind(&candidates).bind(&raw_identity).bind(by)
        .fetch_one(pool).await?;

    let iid = bind_identity(pool, &block, manual_id,
                            chrono::Local::now().date_naive()).await?;
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
