//! Search that costs nothing.
//!
//! Spec §6.1. Every source here is local, so this runs on every keystroke
//! without touching the daily hit budget. The corpus grows monotonically: every
//! Bloomberg search and every resolution adds rows that make the next search
//! better, and none of that growth costs a second call.
//!
//! Four sources, in decreasing strength:
//!   book_entry.label       instruments the user actually holds
//!   instrument_alias.value every identifier ever worn, current or historical
//!   instrument_attr.value  the 'name' attribute
//!   instrument_candidate   everything Bloomberg has ever returned from a search
//!
//! Spec §6.1 describes one denormalised search_text column. Indexing each source
//! in place and combining them here is used instead: a materialised view would
//! be stale between refreshes -- a freshly added book entry would not be
//! findable -- and denormalisation triggers on four tables are more machinery
//! than the query saves.

use crate::error::AppResult;
use crate::master_fetch::MasterFetcher;
use crate::resolution::score::Candidate;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Below this, trigram similarity is noise. Tuned so that "AAPL" reaches
/// "AAPL US Equity" (similarity ~0.33) but not "APPLIED MATERIALS"
/// (similarity ~0.05) -- verified against a live pg_trgm in psql, not guessed.
pub const MIN_SIMILARITY: f32 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// In your book.
    Book,
    /// A known instrument, not currently held.
    Instrument,
    /// Seen before in a Bloomberg search, never resolved.
    Candidate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub origin: Origin,
    pub security: Option<String>,
    pub display: String,
    pub description: String,
    pub instrument_id: Option<i64>,
    pub similarity: f32,
}

#[derive(sqlx::FromRow)]
struct RawHit {
    origin: String,
    security: Option<String>,
    display: String,
    description: String,
    instrument_id: Option<i64>,
    similarity: f32,
}

// `%` is pg_trgm's similarity operator (NOT sqlx bind-parameter syntax -- sqlx
// placeholders are always $1, $2, ... and the runtime `query`/`query_as` API
// forwards the SQL string to Postgres verbatim, with no client-side escaping
// of `%`; doubling it to `%%` was tried against a live database and Postgres
// rejects it outright with "operator does not exist: text %% text", so a
// single `%` is what's actually required here).
const SEARCH_SQL: &str = r#"
WITH hits AS (
  -- rank orders the origins so DISTINCT ON keeps the strongest one per
  -- security: an instrument you hold should never be presented as merely
  -- "seen before" because the candidate cache also has it. That guarantee
  -- only works if the book branch itself can be found by the security you
  -- typed, not only by its label -- otherwise typing a held instrument's own
  -- ticker would surface it as Origin::Instrument, defeating the whole point
  -- of labelling by origin. cur.security is computed once via LATERAL and
  -- reused for both the WHERE and the similarity, rather than repeating the
  -- subquery.
  SELECT 'book' AS origin, 1 AS rank,
         cur.security AS security,
         b.label AS display, '' AS description,
         b.instrument_id,
         GREATEST(similarity(b.label, $1),
                  COALESCE(similarity(cur.security, $1), 0)) AS similarity
    FROM book_entry b
    LEFT JOIN LATERAL (
      SELECT a.value AS security FROM instrument_alias a
       WHERE a.instrument_id = b.instrument_id
         AND a.id_type = 'bdp_security'
         AND a.valid_to > CURRENT_DATE
         AND a.system_to = 'infinity'
       ORDER BY a.valid_from DESC LIMIT 1
    ) cur ON true
   WHERE b.label % $1 OR (cur.security IS NOT NULL AND cur.security % $1)

  UNION ALL
  -- No valid_to filter here on purpose: a ticker the instrument used to wear
  -- (closed valid_to, but still the row we currently believe, system_to
  -- infinity) must remain findable by the identifier a user actually types.
  SELECT 'instrument', 2,
         a.value, a.value, '', a.instrument_id, similarity(a.value, $1)
    FROM instrument_alias a
   WHERE a.system_to = 'infinity' AND a.value % $1

  UNION ALL
  SELECT 'instrument', 3,
         NULL::text, t.value, '', t.instrument_id, similarity(t.value, $1)
    FROM instrument_attr t
   WHERE t.attr = 'name' AND t.system_to = 'infinity' AND t.value % $1

  UNION ALL
  SELECT 'candidate', 4,
         c.security, c.security, c.description, c.instrument_id,
         greatest(similarity(c.security, $1), similarity(c.description, $1))
    FROM instrument_candidate c
   WHERE c.security % $1 OR c.description % $1
),
strong AS (
  SELECT * FROM hits WHERE similarity >= $2
),
best AS (
  -- instrument_id is part of the key, not just the ordering: two DIFFERENT
  -- instruments can legitimately wear the same security string (the BMW case
  -- in store.rs) or carry the same label, and keying on the string alone
  -- collapsed them into a single row -- hiding from the user the very
  -- ambiguity the resolution engine would then send to review. Rows with no
  -- instrument_id (candidate-cache entries) still collapse by string, which
  -- is what that key is for.
  SELECT DISTINCT ON (coalesce(security, display), instrument_id)
         origin, security, display, description, instrument_id, similarity
    FROM strong
   ORDER BY coalesce(security, display), instrument_id, rank, similarity DESC
)
SELECT origin, security, display, description, instrument_id, similarity
  FROM best ORDER BY similarity DESC, display LIMIT $3
"#;

pub async fn local(pool: &PgPool, query: &str, limit: i64) -> AppResult<Vec<SearchHit>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }

    // pg_trgm's `%` operator -- what lets the WHERE clauses above actually use
    // the GIN trigram indexes instead of a sequential scan -- has its own
    // session-level threshold (pg_trgm.similarity_threshold), defaulting to
    // 0.3. That default is HIGHER than MIN_SIMILARITY (0.25): left alone, the
    // `%` prefilter would silently discard rows in [0.25, 0.3) before the
    // explicit `similarity >= $2` check ever saw them, making MIN_SIMILARITY a
    // lie for anything in that band. Lowering the operator's own threshold
    // makes it agree with the constant this module actually promises.
    //
    // Done with `set_config(..., is_local => true)` inside a transaction, NOT
    // with `set_limit()`. `set_limit` mutates the SESSION, and a pooled
    // connection outlives this function: the mutated GUC went back to the pool
    // and any later, unrelated consumer of that connection inherited a
    // threshold it never asked for. `SET LOCAL` semantics unwind at COMMIT.
    // (`set_config` rather than the `SET LOCAL` statement because only the
    // function form takes a bind parameter.)
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('pg_trgm.similarity_threshold', $1, true)")
        .bind(MIN_SIMILARITY.to_string())
        .execute(&mut *tx)
        .await?;

    let rows = sqlx::query_as::<_, RawHit>(SEARCH_SQL)
        .bind(q)
        .bind(MIN_SIMILARITY)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(rows.into_iter().map(|r| SearchHit {
        origin: match r.origin.as_str() {
            "book" => Origin::Book,
            "instrument" => Origin::Instrument,
            _ => Origin::Candidate,
        },
        security: r.security,
        display: r.display,
        description: r.description,
        instrument_id: r.instrument_id,
        similarity: r.similarity,
    }).collect())
}

/// Keep every row Bloomberg has ever returned. One search for "AAPL" seeds all
/// its listings permanently, which is what makes the local tier good enough to
/// make the Bloomberg tier rare.
pub async fn remember_candidates(pool: &PgPool, cands: &[Candidate]) -> AppResult<usize> {
    let mut n = 0;
    for c in cands {
        sqlx::query(
            "INSERT INTO instrument_candidate
               (security, raw_security, description, yellow_key)
             VALUES ($1,$1,$2,$3)
             ON CONFLICT (security) DO UPDATE
               SET last_seen = now(),
                   description = CASE WHEN EXCLUDED.description <> ''
                                      THEN EXCLUDED.description
                                      ELSE instrument_candidate.description END")
            .bind(&c.security).bind(&c.description)
            .bind(c.security.rsplit(' ').next())
            .execute(pool).await?;
        n += 1;
    }
    Ok(n)
}

/// Once a candidate becomes a real instrument, say so, so search can show it as
/// known rather than merely seen. Task 9 (resolution binding) calls this.
pub async fn link_candidate(pool: &PgPool, security: &str, instrument_id: i64) -> AppResult<()> {
    sqlx::query("UPDATE instrument_candidate SET instrument_id = $2 WHERE security = $1")
        .bind(security).bind(instrument_id).execute(pool).await?;
    Ok(())
}

/// The Bloomberg search tier. Spec §6.2: one button, one call, cached forever.
/// This is the ONLY place in the search feature allowed to reach Bloomberg --
/// nothing in `search::local`'s callers may route typing, focus or navigation
/// here. `query` is trimmed and, if empty, the function returns without
/// touching the fetcher at all: an accidental call with nothing typed must
/// not spend a hit.
#[derive(Debug, Serialize)]
pub struct BloombergSearch {
    pub hits: Vec<SearchHit>,
    pub estimated_hits: i64,
    pub cached: usize,
}

pub async fn bloomberg<F: MasterFetcher>(
    pool: &PgPool, fetcher: &F, query: &str, yellow_key: &str,
) -> AppResult<BloombergSearch> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(BloombergSearch { hits: Vec::new(), estimated_hits: 0, cached: 0 });
    }

    let filter = crate::resolution::engine::yellow_key_filter(yellow_key);
    // The `hit_ledger` write that used to live here is gone: it is done at the
    // wire seam instead (`BlpapiMasterFetcher::instrument_list`), where a
    // future call site cannot forget it. This one had already been forgotten
    // four times over in `resolution`.
    let answered = fetcher.instrument_list(q, filter, 20).await?;

    // `answered.parsed` is already normalised by `parse_list` --
    // "AAPL US<equity>" became "AAPL US Equity" on the way in, so the raw
    // Bloomberg form can never reach instrument_candidate.security. Pasting
    // the raw form there once required a database migration (0004) to
    // repair; `remember_candidates` must never be handed `answered.raw`.
    let cached = remember_candidates(pool, &answered.parsed).await?;

    // Answer from the local tier so the caller sees one consistent shape,
    // with book and known-instrument results ranked above the new arrivals
    // this call just cached.
    let hits = local(pool, q, 20).await?;
    Ok(BloombergSearch {
        hits,
        estimated_hits: crate::budget::SEARCH_HIT_COST,
        cached,
    })
}
