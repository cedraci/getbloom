use crate::error::AppResult;
use crate::fetch::{CellValue, FetchOutcome};
use serde::Serialize;
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize)]
pub struct IngestSummary {
    pub inserted: u64,
    pub superseded: u64,
    pub unchanged: u64,
    pub issues: u64,
}

/// Write observations without ever destroying one.
///
/// The previous implementation ended in ON CONFLICT DO UPDATE, which silently
/// replaced yesterday's number with today's. That makes a corrected value
/// indistinguishable from an original one and makes point-in-time history
/// impossible. Here a changed value closes the old row's system_to and inserts
/// a new one beneath it; an unchanged value does nothing at all.
pub async fn ingest_outcome(pool: &PgPool, run_id: i64, outcome: &FetchOutcome)
    -> AppResult<IngestSummary>
{
    // The basis these values were actually fetched at: all four adjustment
    // flags false (see blp_fetch.build_request).
    let raw_basis: i16 = sqlx::query_scalar(
        "SELECT id FROM adjustment_basis
          WHERE adj_normal = false AND adj_abnormal = false
            AND adj_split = false AND adj_follow_dpdf = false")
        .fetch_one(pool).await?;

    let mut tx = pool.begin().await?;
    let (mut inserted, mut superseded, mut unchanged) = (0u64, 0u64, 0u64);

    for c in &outcome.cells {
        // Only a numeric price has an adjustment basis (schema
        // observation_numeric_needs_basis / the migration's "text-valued
        // fields ... legitimately have none"). Asserting RAW for a text cell
        // would be a false claim, and would also let a text row and a future
        // NULL-basis writer both claim "current" for the same logical series
        // without colliding on observation_current.
        let (num, text, basis_id) = match &c.value {
            CellValue::Num(n) => (Some(*n), None, Some(raw_basis)),
            CellValue::Text(t) => (None, Some(t.clone()), None),
        };

        // FOR UPDATE: two concurrent runs racing the same
        // (instrument, field, date, ..., basis) key must serialize here,
        // not both decide "no current row" and collide on
        // observation_current.
        let current: Option<(i64, Option<f64>, Option<String>)> = sqlx::query_as(
            "SELECT id, value_num, value_text FROM observation
              WHERE instrument_id = $1 AND field_id = $2 AND obs_date = $3
                AND granularity = 'eod' AND layer = 'raw'
                AND basis_id IS NOT DISTINCT FROM $4
                AND system_to = 'infinity'
              FOR UPDATE")
            .bind(c.instrument_id).bind(c.field_id).bind(c.obs_date).bind(basis_id)
            .fetch_optional(&mut *tx).await?;

        if let Some((id, old_num, old_text)) = current {
            if old_num == num && old_text == text {
                unchanged += 1;
                continue;
            }
            sqlx::query("UPDATE observation SET system_to = now() WHERE id = $1")
                .bind(id).execute(&mut *tx).await?;
            superseded += 1;
        }

        sqlx::query(
            "INSERT INTO observation
               (instrument_id, field_id, obs_date, granularity, layer, basis_id,
                value_num, value_text, run_id)
             VALUES ($1,$2,$3,'eod','raw',$4,$5,$6,$7)")
            .bind(c.instrument_id).bind(c.field_id).bind(c.obs_date)
            .bind(basis_id).bind(num).bind(text).bind(run_id)
            .execute(&mut *tx).await?;
        inserted += 1;
    }

    for p in &outcome.problems {
        sqlx::query(
            "INSERT INTO ingest_issue
               (run_id, instrument_id, field_id, obs_date, severity, code, detail)
             VALUES ($1,$2,$3,$4,'warn',$5,$6)")
            .bind(run_id).bind(p.instrument_id).bind(p.field_id).bind(p.obs_date)
            .bind(&p.code).bind(&p.detail)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(IngestSummary { inserted, superseded, unchanged,
                       issues: outcome.problems.len() as u64 })
}
