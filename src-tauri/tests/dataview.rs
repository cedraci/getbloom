mod common;

use common::uniq;
use getbloomdata_lib::dataview;
use getbloomdata_lib::fetch::{CellValue, FetchOutcome, ObsCell};
use getbloomdata_lib::ingest;
use getbloomdata_lib::instrument::store::{self, NewAlias};

fn d(s: &str) -> chrono::NaiveDate { s.parse().unwrap() }

/// Instrument + class + book entry + numeric field + run.
async fn scaffold(pool: &sqlx::PgPool, stem: &str) -> (i64, i64, i64) {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("DVClass")).fetch_one(pool).await.unwrap();
    let inst = store::create(pool).await.unwrap();
    let iid = inst.instrument_id;
    let sec = format!("{} US Equity", uniq(stem));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, iid, &NewAlias {
        id_type: "bdp_security".into(), value: sec.clone(),
        exch_code: Some("US".into()), valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(iid).bind(class).bind(&sec).execute(pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'PX_LAST','Last price','numeric') RETURNING id")
        .bind(class).fetch_one(pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("dvview")).fetch_one(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();
    (iid, fid, rid)
}

fn one_cell(iid: i64, fid: i64, date: &str, v: f64) -> FetchOutcome {
    FetchOutcome { cells: vec![ObsCell { instrument_id: iid, field_id: fid,
        obs_date: d(date), value: CellValue::Num(v) }], problems: vec![] }
}

/// The read side must show a correction as history: superseded rows on
/// demand, only the current one by default, each numeric row naming its
/// adjustment basis.
#[tokio::test]
#[ignore = "requires postgres"]
async fn observations_show_current_by_default_and_history_on_demand() {
    let pool = common::pool().await;
    let (iid, fid, rid) = scaffold(&pool, "DVONE").await;
    ingest::ingest_outcome(&pool, rid, &one_cell(iid, fid, "2026-08-18", 499.23))
        .await.unwrap();
    ingest::ingest_outcome(&pool, rid, &one_cell(iid, fid, "2026-08-18", 124.81))
        .await.unwrap();

    let current = dataview::observations(&pool, iid, fid, false, 100).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].value_num, Some(124.81));
    assert!(current[0].current);
    assert!(current[0].basis_note.as_deref().unwrap_or("").starts_with("RAW"),
            "a price without its basis is not a fact: {:?}", current[0].basis_note);

    let all = dataview::observations(&pool, iid, fid, true, 100).await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all[0].current && !all[1].current,
            "newest belief first for the same date: {all:?}");
    assert_eq!(all[1].value_num, Some(499.23), "the superseded value is visible");
}

/// The verbatim Bloomberg payload must reach the UI byte-for-byte -- it is
/// the column the accuracy check against the Terminal happens on.
#[tokio::test]
#[ignore = "requires postgres"]
async fn corp_actions_full_carries_the_payload_verbatim() {
    let pool = common::pool().await;
    let (iid, _fid, _rid) = scaffold(&pool, "DVTWO").await;
    sqlx::query(
        "INSERT INTO corp_action
           (instrument_id, source_field, natural_key, event_date, amount, payload)
         VALUES ($1,'DVD_HIST_ALL_WITH_AMT_STATUS','2026-08-10|Regular Cash',
                 '2026-08-10',0.26,
                 '{\"Ex-Date\":\"2026-08-10\",\"Dividend Amount\":0.26,\"Odd, Column\":\"x\"}'::jsonb)")
        .bind(iid).execute(&pool).await.unwrap();

    let rows = dataview::corp_actions_full(&pool, iid, false).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].current);
    assert_eq!(rows[0].payload["Dividend Amount"], 0.26);
    assert_eq!(rows[0].payload["Odd, Column"], "x", "payload is verbatim");
    assert_eq!(rows[0].natural_key, "2026-08-10|Regular Cash");
}

/// CSV export: header + one line per current row, quoting fields that carry
/// commas or quotes (the payload JSON always does).
#[tokio::test]
#[ignore = "requires postgres"]
async fn csv_exports_write_headers_rows_and_quote_correctly() {
    let pool = common::pool().await;
    let (iid, fid, rid) = scaffold(&pool, "DVTHREE").await;
    ingest::ingest_outcome(&pool, rid, &one_cell(iid, fid, "2026-08-17", 100.0))
        .await.unwrap();
    ingest::ingest_outcome(&pool, rid, &one_cell(iid, fid, "2026-08-18", 101.5))
        .await.unwrap();
    sqlx::query(
        "INSERT INTO corp_action
           (instrument_id, source_field, natural_key, event_date, amount, payload)
         VALUES ($1,'EQY_DVD_ADJUST_FACT','2020-08-31|1|3','2020-08-31',4.0,
                 '{\"Adjustment Date\":\"2020-08-31\",\"Adjustment Factor\":4.0}'::jsonb)")
        .bind(iid).execute(&pool).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let obs_path = dir.path().join("obs.csv");
    let n = dataview::export_observations_csv(&pool, iid, fid, &obs_path).await.unwrap();
    assert_eq!(n, 2);
    let text = std::fs::read_to_string(&obs_path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "header + 2 rows: {text}");
    assert_eq!(lines[0], "obs_date,value,basis,run_id,recorded_at");
    assert!(lines[1].starts_with("2026-08-18,101.5,"), "newest first: {}", lines[1]);

    let ca_path = dir.path().join("ca.csv");
    let n = dataview::export_corp_actions_csv(&pool, iid, &ca_path).await.unwrap();
    assert_eq!(n, 1);
    let text = std::fs::read_to_string(&ca_path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("source_field,event_date,amount,"));
    assert!(lines[1].contains("\"{\"\""),
            "the JSON payload must be quoted with doubled inner quotes: {}", lines[1]);
}
