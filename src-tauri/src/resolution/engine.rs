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
use crate::resolution::normalize::{build_security, detect_id_kind, normalize_bbg_security};
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

/// The attributes an identity block carries, and the `instrument_attr.attr`
/// each belongs under. Extracted so creation and reconciliation cannot drift:
/// an attribute added here is refreshed on every later resolution for free.
///
/// No "status": P0 §10.2 -- SIMP_SEC_STATUS is a trading-session state, not a
/// lifecycle one. INACTIVE_DATE closes the validity periods instead -- for
/// aliases via their `valid_to`, and for attributes via `close_attrs_at` --
/// which is the durable way to say an instrument has ended.
fn attr_pairs(block: &IdentityBlock) -> [(&'static str, &Option<String>); 6] {
    [
        ("name", &block.name),
        ("exchange", &block.exch_code),
        ("currency", &block.currency),
        ("country", &block.country),
        ("instrument_type", &block.security_typ2),
        ("asset_class", &block.market_sector),
    ]
}

/// Write every attribute the block carries for the period starting at `from`,
/// then cap everything at INACTIVE_DATE if the instrument has died.
///
/// `set_attr` is already correction-aware: a row for this exact `valid_from`
/// carrying a different value is superseded and re-inserted; an identical
/// value is a no-op. That is precisely what "refresh" means, so creation and
/// reconciliation share this function unchanged rather than each writing their
/// own loop that can drift from the other.
async fn write_attrs_tx(tx: &mut store::Tx<'_>, instrument_id: i64,
                        block: &IdentityBlock, from: NaiveDate, decision_id: i64)
    -> AppResult<()>
{
    for (attr, value) in attr_pairs(block) {
        if let Some(v) = value {
            store::set_attr(tx, instrument_id, attr, v, from,
                            "bloomberg", Some(decision_id)).await?;
        }
    }
    if let Some(inactive) = block.inactive_date {
        store::close_attrs_at(tx, instrument_id, inactive).await?;
    }
    Ok(())
}

/// The instrument already exists and Bloomberg has just answered about it
/// again. Bring its identity up to date instead of returning it untouched.
///
/// This is the branch's headline promise, and until this function existed no
/// production path delivered it. `bind_identity` was bind-or-return-existing:
/// an instrument bound while it wore `FB US Equity` went on answering
/// `FB US Equity` forever, because a later resolution of `META US Equity`
/// found the same FIGI and returned before any alias or attribute was
/// written. `current_security` kept handing a dead ticker to every fetch and
/// the series stopped without one error.
///
/// It also makes a rename discoverable WITHOUT `HISTORICAL_IDS_TIME_RANGE`,
/// which is the real answer to that field's bootstrap problem (P0 §6.5): you
/// cannot anchor a chain you have not already seen, but you can notice that
/// the FIGI you already hold now answers to a different security string.
///
/// The rename is recorded the only way this codebase records one: the current
/// `bdp_security` period is CLOSED at today and a new period is INSERTED. No
/// UPDATE ever touches `value`. The attributes then run through the same
/// `write_attrs_tx` the creation path uses, so a name or exchange change
/// arriving alongside the rename is not silently dropped either.
///
/// One transaction: a half-applied rename -- old alias closed, new one missing
/// -- would leave the instrument with no security valid today, which is worse
/// than the stale ticker it replaces.
async fn reconcile_identity(pool: &PgPool, instrument_id: i64, block: &IdentityBlock,
                            decision_id: i64, as_of: NaiveDate) -> AppResult<i64>
{
    let (from, _to) = validity_period(block.listing_date, block.inactive_date, as_of);
    let mut tx = pool.begin().await?;

    let current: Option<(i64, String, NaiveDate)> = sqlx::query_as(
        "SELECT id, value, valid_from FROM instrument_alias
          WHERE instrument_id = $1 AND id_type = 'bdp_security'
            AND system_to = 'infinity' AND valid_from <= $2 AND valid_to > $2
          ORDER BY valid_from DESC LIMIT 1")
        .bind(instrument_id).bind(as_of).fetch_optional(&mut *tx).await?;

    let incoming = block.security.trim();
    let already_current = current.as_ref()
        .is_some_and(|(_, v, _)| v.eq_ignore_ascii_case(incoming));

    if !incoming.is_empty() && !already_current {
        if let Some((alias_id, _, valid_from)) = current {
            if valid_from < as_of {
                // A real-world change: the old string was true until today.
                store::close_alias(&mut tx, alias_id, as_of).await?;
            } else {
                // The period we would close starts today or later, so closing
                // it at today would produce an empty range and overlap its own
                // replacement. This is a correction of a belief formed today,
                // which is what system-time supersession is for.
                store::supersede_alias(&mut tx, alias_id).await?;
            }
        }
        store::insert_alias(&mut tx, instrument_id, &NewAlias {
            id_type: "bdp_security".into(), value: incoming.to_string(),
            exch_code: block.exch_code.clone(),
            valid_from: as_of, valid_to: None,
            source: "bloomberg_ref".into(), bbg_action_id: None,
            anchoring_identifier: None,
        }).await?;
    }

    write_attrs_tx(&mut tx, instrument_id, block, from, decision_id).await?;
    tx.commit().await?;
    Ok(instrument_id)
}

/// Write an identity block into the master: one instrument, its aliases, its
/// attributes. On re-resolution the instrument is never duplicated -- and, as
/// of the C1 fix, never left stale either:
/// - a FIGI already in the master identifies the same instrument, not a new
///   one -- the common case, since almost every IDENTITY_FIELDS response
///   carries one -- and the block Bloomberg just returned is reconciled onto
///   it (`reconcile_identity`), which is how a rename actually lands;
/// - when there is no FIGI (the resolve_review fallback path), the same
///   bdp_security alias identifies it instead, so a double-submit or two
///   reviews
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
            return reconcile_identity(pool, existing, block, decision_id, as_of).await;
        }
    } else if let Some(existing) = store::find_by_alias(
        pool, "bdp_security", &block.security, as_of).await?
    {
        return reconcile_identity(pool, existing, block, decision_id, as_of).await;
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
        // Its own id_type, not 'figi'. See migration 0001's comment on the
        // id_type domain: these are two identifiers of two different things,
        // and writing both as 'figi' gave one instrument two simultaneous
        // 'figi' values over one period -- which the alias non-overlap fence
        // now refuses outright.
        store::insert_alias(&mut tx, inst.instrument_id,
                            &alias("share_class_figi", v)).await?;
    }
    if let Some(v) = &block.isin {
        store::insert_alias(&mut tx, inst.instrument_id, &alias("isin", v)).await?;
    }
    if let Some(v) = &block.bbg_unique {
        store::insert_alias(&mut tx, inst.instrument_id, &alias("bbg_unique", v)).await?;
    }

    write_attrs_tx(&mut tx, inst.instrument_id, block, from, decision_id).await?;
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
            // The load-bearing field. The review screen re-points to THIS
            // instrument id; it must never be asked to hand the security
            // string back, because that string may be the placeholder above,
            // which Bloomberg cannot resolve and which must never become an
            // alias value.
            instrument_id: Some(*iid),
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
    // 'share_class_figi' is probed too: bind_identity writes
    // ID_BB_GLOBAL_SHARE_CLASS_LEVEL under its own id_type (see migration
    // 0001), so without it a user pasting a share-class FIGI would fall
    // through to a Bloomberg call for something already in the master. It is
    // probed last because it is the one identifier here that several
    // instruments legitimately share -- every sibling listing in the class --
    // so it reaches the local-ambiguity branch rather than binding one of them.
    for id_type in ["bdp_security", "ticker", "isin", "figi", "share_class_figi"] {
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
                // An OBJECT carrying the list, not the bare list. A bare
                // Vec<Scored> is indistinguishable on the wire from the
                // Bloomberg-candidate list, so the review screen matched it
                // with its scored-list guard and offered "This one" -- which
                // spends a Bloomberg call, and, where the security string was
                // the `instrument #42` placeholder, minted a permanent
                // instrument wearing that literal text. The marker is what
                // lets the screen tell the two apart and offer a free local
                // re-point instead.
                let candidates_json = serde_json::json!({
                    "local_ambiguity": true,
                    "matched": id_type,
                    "bloomberg_calls": 0,
                    "candidates": serde_json::to_value(&scored)
                        .unwrap_or(serde_json::json!([])),
                });
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
        // No HISTORICAL_IDS_TIME_RANGE call here. Spec §5.1 put one on this
        // path; P0 §6.5 measured why it cannot stay. The field is anchored on
        // the identifier the chain STARTED from, and resolution only knows the
        // identifier the chain ENDED at -- so passing the resolved current
        // security returns a well-formed chain belonging to a different
        // company (META -> METV, the Roundhill ETF). The field cannot discover
        // a rename, only confirm one, so it has no place on the path whose job
        // is discovery. `reconcile_identity` above is what actually catches a
        // rename now, off a FIGI we already hold, and costs nothing extra.
        // Identifier history remains available as an explicit user action with
        // a user-supplied anchor: `commands::ingest_identifier_history`.
        //
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
            // No identifier-history call here either -- see step 3 above.
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
///
/// Refuses, too, a `chosen_security` that `resolution::normalize` does not
/// accept as a security string. The UI is not trusted to be the only guard:
/// `local_ambiguity_candidates` puts an `instrument #42` placeholder in the
/// security field when an existing instrument has no current security string,
/// and a UI regression that handed that back would spend a real Bloomberg call
/// on it, then -- since Bloomberg cannot answer -- fall through to the
/// bare-block path and mint a permanent instrument whose `bdp_security` alias
/// is the literal text `instrument #42`. Making the placeholder unbindable
/// here means the UI can only ever regress into an error.
pub async fn resolve_review<F: MasterFetcher>(pool: &PgPool, fetcher: &F, review_id: i64,
                                              chosen_security: &str, by: &str,
                                              as_of: NaiveDate) -> AppResult<i64>
{
    // Before anything else, including before the review is read: a refused
    // request must not cost a Bloomberg hit or a database round trip.
    let Some(sec) = normalize_bbg_security(chosen_security) else {
        return Err(AppError::Validation(format!(
            "{chosen_security:?} is not a security string -- it must end in a \
             Bloomberg market sector (e.g. 'AAPL US Equity'). A locally \
             ambiguous review is re-pointed at an existing instrument, not \
             bound from a string.")));
    };

    let ctx: ReviewContext = sqlx::query_as(
        "SELECT d.id AS decision_id, d.raw_input, r.status, d.candidates
           FROM resolution_review r JOIN resolution_decision d ON d.id = r.decision_id
          WHERE r.id = $1")
        .bind(review_id).fetch_one(pool).await?;
    if ctx.status != "pending" {
        return Err(AppError::Validation(format!(
            "review {review_id} is not pending (status: {})", ctx.status)));
    }

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

    let iid = bind_identity(pool, &block, manual_id, as_of).await?;
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

/// A human resolved a LOCAL ambiguity by pointing at one of the existing
/// instruments the identifier already matched. Costs zero Bloomberg calls, by
/// construction: there is nothing for Bloomberg to answer.
///
/// Spec §7 says so outright -- "a locally ambiguous identifier: none; a
/// Bloomberg call cannot resolve a local ambiguity" -- but until this function
/// existed the review screen had no way to act on that row except
/// `resolve_review`, which always calls out. So the free path was documented,
/// tested at the engine level, and unreachable from the UI.
///
/// Nothing is bound here that was not already bound: the instrument exists and
/// keeps every alias and attribute it had. What is recorded is the human's
/// decision -- a `manual` decision naming the chosen instrument, and the
/// review closed against it -- so the audit trail answers "why is this input
/// pointing at that instrument" the same way every other path does.
///
/// The chosen instrument must be one of the candidates the decision actually
/// recorded. A caller free to name any instrument id could point an input at
/// something the user never saw, and the review would look identical
/// afterwards.
pub async fn resolve_review_local(pool: &PgPool, review_id: i64,
                                  instrument_id: i64, by: &str) -> AppResult<i64>
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

    let offered: Vec<i64> = ctx.candidates.get("candidates")
        .and_then(|c| c.as_array())
        .map(|list| list.iter()
            .filter_map(|s| s.get("instrument_id").and_then(|v| v.as_i64()))
            .collect())
        .unwrap_or_default();
    if !offered.contains(&instrument_id) {
        return Err(AppError::Validation(format!(
            "instrument {instrument_id} was not one of the candidates review \
             {review_id} recorded ({offered:?})")));
    }

    let candidates = serde_json::json!({
        "local_repoint": true,
        "chosen_instrument_id": instrument_id,
        "bloomberg_calls": 0,
        "review_id": review_id,
        "source_decision_id": ctx.decision_id,
        "original_candidates": ctx.candidates,
    });
    let manual_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO resolution_decision
           (raw_input, normalized, method, chosen_instrument_id, candidates, decided_by)
         VALUES ($1,$2,'manual',$3,$4,$5) RETURNING id")
        .bind(&ctx.raw_input).bind(&ctx.raw_input).bind(instrument_id)
        .bind(&candidates).bind(by)
        .fetch_one(pool).await?;

    let done = sqlx::query(
        "UPDATE resolution_review SET status = 'resolved', closed_at = now(),
                note = note || $2
          WHERE id = $1 AND status = 'pending'")
        .bind(review_id)
        .bind(format!(" re-pointed by {by} to existing instrument {instrument_id} \
                        (0 Bloomberg calls)"))
        .execute(pool).await?;
    if done.rows_affected() == 0 {
        return Err(AppError::Validation(format!(
            "review {review_id} stopped being pending before it could be closed")));
    }
    let _ = manual_id;
    Ok(instrument_id)
}

/// Also needed by the UI: a review the user judges unresolvable.
///
/// Guarded on `status = 'pending'` for the same reason `resolve_review` is.
/// Without it, rejecting an already-`resolved` review flipped it to
/// `rejected` and OVERWROTE `note` -- destroying the record of what the
/// binding was and who made it, while the instrument it bound stayed in the
/// book. A `rows_affected` check turns that into an error rather than a
/// silent success, because "nothing happened" and "it worked" must not look
/// the same to the caller.
pub async fn reject_review(pool: &PgPool, review_id: i64, note: &str) -> AppResult<()> {
    let done = sqlx::query("UPDATE resolution_review
                    SET status = 'rejected', closed_at = now(), note = $2
                  WHERE id = $1 AND status = 'pending'")
        .bind(review_id).bind(note).execute(pool).await?;
    if done.rows_affected() == 0 {
        return Err(AppError::Validation(format!(
            "review {review_id} is not pending; it cannot be rejected")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Post-run recovery of dead securities
// ---------------------------------------------------------------------------

/// One probe per instrument per this many days: a permanently dead
/// instrument (delisted, no successor) would otherwise cost 12 hits every
/// single day forever.
const AUTO_RERESOLVE_COOLDOWN_DAYS: i32 = 7;

async fn note_auto_issue(pool: &PgPool, run_id: i64, iid: i64, code: &str,
                         detail: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO ingest_issue (run_id, instrument_id, severity, code, detail)
         VALUES ($1,$2,'warn',$3,$4)")
        .bind(run_id).bind(iid).bind(code).bind(detail)
        .execute(pool).await?;
    Ok(())
}

/// After a run, re-point instruments whose security Bloomberg rejected.
///
/// The dead string cannot be resolved -- it is dead. The FIGI can: it is the
/// one identifier a rename never touches. `/bbgid/<figi>` is the
/// parsekeyable form and needs no yellow key. The answer lands through
/// `reconcile_identity`, i.e. exactly the close-and-insert a manual
/// re-resolution performs; nothing here can mint a second instrument.
///
/// Called only from the LIVE wrappers (`orchestrator::run_eod`,
/// `run_backfill`); every outcome, including the skips, is written to
/// `ingest_issue` so the run screen shows what happened and the cooldown has
/// a record to key on. Returns how many instruments were re-pointed.
pub async fn auto_reresolve_invalid<F: MasterFetcher>(
    pool: &PgPool, fetcher: &F, run_id: i64, as_of: NaiveDate) -> AppResult<u32>
{
    let dead: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT instrument_id FROM ingest_issue
          WHERE run_id = $1 AND code = 'invalid_security'
            AND instrument_id IS NOT NULL")
        .bind(run_id).fetch_all(pool).await?;

    let mut repointed = 0u32;
    for iid in dead {
        let probed_recently: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ingest_issue
              WHERE instrument_id = $1 AND code LIKE 'auto_reresolve%'
                AND created_at > now() - make_interval(days => $2)")
            .bind(iid).bind(AUTO_RERESOLVE_COOLDOWN_DAYS).fetch_one(pool).await?;
        if probed_recently > 0 {
            continue;
        }

        let figi: Option<String> = sqlx::query_scalar(
            "SELECT id_bb_global FROM instrument WHERE instrument_id = $1")
            .bind(iid).fetch_one(pool).await?;
        let Some(figi) = figi else {
            note_auto_issue(pool, run_id, iid, "auto_reresolve_skipped",
                            "no FIGI on record").await?;
            continue;
        };

        let probe = format!("/bbgid/{figi}");
        let answered = match fetcher.identity(&[probe.clone()]).await {
            Ok(a) => a,
            Err(e) => {
                note_auto_issue(pool, run_id, iid, "auto_reresolve_failed",
                                &format!("identity probe failed: {e}")).await?;
                continue;
            }
        };
        let Some(block) = answered.parsed.first() else {
            note_auto_issue(pool, run_id, iid, "auto_reresolve_no_answer",
                            &format!("Bloomberg returned nothing for {probe}")).await?;
            continue;
        };
        // The probe asked about OUR figi; an answer wearing another one (or
        // none) must not be reconciled onto this instrument.
        if block.figi.as_deref() != Some(figi.as_str()) {
            note_auto_issue(pool, run_id, iid, "auto_reresolve_mismatch",
                            &format!("probe {probe} answered with figi {:?}",
                                     block.figi)).await?;
            continue;
        }

        let input = ResolveInput {
            raw: probe.clone(), yellow_key: String::new(),
            hints: Hints::default(), as_of, decided_by: "auto".into(),
        };
        let decision_id = record_decision(pool, &input, &probe, "auto_reresolve",
                                          Some(iid), &serde_json::json!([]),
                                          Some(&answered.raw)).await?;
        reconcile_identity(pool, iid, block, decision_id, as_of).await?;
        note_auto_issue(pool, run_id, iid, "auto_reresolve",
                        &format!("re-pointed to {}", block.security)).await?;
        repointed += 1;
    }
    Ok(repointed)
}
