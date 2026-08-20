//! The read side of the store: what is actually in the database, rendered
//! for the Data tab and exported as CSV, so accuracy can be checked against
//! the Terminal without psql.
//!
//! Read-only over existing tables. Nothing here calls Bloomberg, ever.

use crate::error::AppResult;
use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use sqlx::PgPool;
use std::path::Path;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ObsRow {
    pub id: i64,
    pub obs_date: NaiveDate,
    pub value_num: Option<f64>,
    pub value_text: Option<String>,
    /// The adjustment_basis note (e.g. "RAW - ..."); None for text fields.
    pub basis_note: Option<String>,
    pub layer: String,
    pub run_id: i64,
    pub system_from: DateTime<Utc>,
    pub current: bool,
}

/// Stored observations for one (instrument, field), newest date first; for
/// one date, the newest belief first so a correction reads top-down.
pub async fn observations(pool: &PgPool, instrument_id: i64, field_id: i64,
                          include_superseded: bool, limit: i64)
    -> AppResult<Vec<ObsRow>> {
    Ok(sqlx::query_as::<_, ObsRow>(
        "SELECT o.id, o.obs_date, o.value_num, o.value_text,
                b.note AS basis_note, o.layer, o.run_id, o.system_from,
                (o.system_to = 'infinity') AS current
           FROM observation o
           LEFT JOIN adjustment_basis b ON b.id = o.basis_id
          WHERE o.instrument_id = $1 AND o.field_id = $2
            AND ($3 OR o.system_to = 'infinity')
          ORDER BY o.obs_date DESC, o.system_from DESC
          LIMIT $4")
        .bind(instrument_id).bind(field_id).bind(include_superseded)
        .bind(limit.clamp(1, 5000))
        .fetch_all(pool).await?)
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CorpActionFull {
    pub id: i64,
    pub source_field: String,
    pub natural_key: String,
    pub event_date: Option<NaiveDate>,
    pub amount: Option<f64>,
    pub operator: Option<i16>,
    pub flag: Option<i16>,
    pub dvd_type: Option<String>,
    pub frequency: Option<String>,
    pub declared_date: Option<NaiveDate>,
    pub record_date: Option<NaiveDate>,
    pub pay_date: Option<NaiveDate>,
    pub amount_status: Option<String>,
    /// Verbatim Bloomberg row -- the column accuracy checks happen on.
    pub payload: serde_json::Value,
    pub system_from: DateTime<Utc>,
    pub current: bool,
}

pub async fn corp_actions_full(pool: &PgPool, instrument_id: i64,
                               include_superseded: bool)
    -> AppResult<Vec<CorpActionFull>> {
    Ok(sqlx::query_as::<_, CorpActionFull>(
        "SELECT id, source_field, natural_key, event_date, amount, operator,
                flag, dvd_type, frequency, declared_date, record_date,
                pay_date, amount_status, payload, system_from,
                (system_to = 'infinity') AS current
           FROM corp_action
          WHERE instrument_id = $1 AND ($2 OR system_to = 'infinity')
          ORDER BY event_date DESC NULLS LAST, id DESC")
        .bind(instrument_id).bind(include_superseded)
        .fetch_all(pool).await?)
}

// ------------------------------------------------------------------ CSV

/// RFC-4180 quoting: wrap when the field carries a comma, quote or newline;
/// double the inner quotes. No dependency for six lines of logic.
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn csv_line(fields: &[String]) -> String {
    fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",")
}

fn opt<T: ToString>(v: &Option<T>) -> String {
    v.as_ref().map(|x| x.to_string()).unwrap_or_default()
}

/// Current observations only (the CSV is for comparing against the Terminal,
/// which has no notion of our superseded beliefs). Returns rows written.
pub async fn export_observations_csv(pool: &PgPool, instrument_id: i64,
                                     field_id: i64, path: &Path)
    -> AppResult<u64> {
    let rows = observations(pool, instrument_id, field_id, false, 5000).await?;
    let mut out = String::from("obs_date,value,basis,run_id,recorded_at\n");
    for r in &rows {
        let value = r.value_num.map(|n| n.to_string())
            .or_else(|| r.value_text.clone()).unwrap_or_default();
        out.push_str(&csv_line(&[
            r.obs_date.to_string(),
            value,
            r.basis_note.clone().unwrap_or_default(),
            r.run_id.to_string(),
            r.system_from.to_rfc3339(),
        ]));
        out.push('\n');
    }
    std::fs::write(path, out)?;
    Ok(rows.len() as u64)
}

pub async fn export_corp_actions_csv(pool: &PgPool, instrument_id: i64,
                                     path: &Path) -> AppResult<u64> {
    let rows = corp_actions_full(pool, instrument_id, false).await?;
    let mut out = String::from(
        "source_field,event_date,amount,operator,flag,dvd_type,frequency,\
         declared_date,record_date,pay_date,amount_status,natural_key,payload_json\n");
    for r in &rows {
        out.push_str(&csv_line(&[
            r.source_field.clone(),
            opt(&r.event_date),
            opt(&r.amount),
            opt(&r.operator),
            opt(&r.flag),
            opt(&r.dvd_type),
            opt(&r.frequency),
            opt(&r.declared_date),
            opt(&r.record_date),
            opt(&r.pay_date),
            opt(&r.amount_status),
            r.natural_key.clone(),
            r.payload.to_string(),
        ]));
        out.push('\n');
    }
    std::fs::write(path, out)?;
    Ok(rows.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quoting_doubles_inner_quotes_and_wraps_commas() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_line(&["a,b".into(), "c".into()]), "\"a,b\",c");
    }
}
