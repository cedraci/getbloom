use crate::error::AppResult;
use crate::fetch::{CellValue, FetchOutcome};
use serde::Serialize;
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize)]
pub struct IngestSummary {
    pub upserted: u64,
    pub issues: u64,
}

pub async fn ingest_outcome(pool: &PgPool, run_id: i64, outcome: &FetchOutcome)
    -> AppResult<IngestSummary> {
    let mut tx = pool.begin().await?;
    let mut upserted = 0u64;
    for c in &outcome.cells {
        let (num, text) = match &c.value {
            CellValue::Num(n) => (Some(*n), None),
            CellValue::Text(t) => (None, Some(t.clone())),
        };
        sqlx::query(
            "INSERT INTO observation
               (asset_id, field_id, obs_date, value_num, value_text, run_id)
             VALUES ($1,$2,$3,$4,$5,$6)
             ON CONFLICT (asset_id, field_id, obs_date) DO UPDATE
               SET value_num = EXCLUDED.value_num,
                   value_text = EXCLUDED.value_text,
                   run_id = EXCLUDED.run_id,
                   ingested_at = now()")
            .bind(c.asset_id).bind(c.field_id).bind(c.obs_date)
            .bind(num).bind(text).bind(run_id)
            .execute(&mut *tx).await?;
        upserted += 1;
    }
    for p in &outcome.problems {
        sqlx::query(
            "INSERT INTO ingest_issue
               (run_id, asset_id, field_id, obs_date, severity, code, detail)
             VALUES ($1,$2,$3,$4,'warn',$5,$6)")
            .bind(run_id).bind(p.asset_id).bind(p.field_id).bind(p.obs_date)
            .bind(&p.code).bind(&p.detail)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(IngestSummary { upserted, issues: outcome.problems.len() as u64 })
}
