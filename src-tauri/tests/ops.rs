//! P10 production-ops behaviours: honest ledger, gap backfill, current_eod.
mod common;
use common::uniq;
use chrono::NaiveDate;
fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

/// The wire shape `master_fetch::parse_identity` reads: one securityData
/// entry, no FIGI, no errors. Mirrors `db_integration.rs`'s fixture helper --
/// fixtures go through the real `book::add` path with Bloomberg mocked, never
/// by hand-seeding `instrument_alias` rows directly.
fn identity_raw_for(security: &str) -> serde_json::Value {
    serde_json::json!([{"securityData": [{
        "security": security,
        "fieldExceptions": [], "sequenceNumber": 0,
        "fieldData": {}
    }]}])
}

/// Build a book entry through `book::add`, with Bloomberg mocked to answer
/// with exactly one identity block for `{ticker} Equity`.
async fn add_book_entry(
    pool: &sqlx::PgPool,
    class_id: i64,
    label: &str,
    ticker: &str,
) -> getbloomdata_lib::book::BookEntry {
    use getbloomdata_lib::book::{self, AddOutcome, AddToBook};
    use getbloomdata_lib::master_fetch::MockMasterFetcher;
    use getbloomdata_lib::resolution::score::Hints;

    let security = format!("{ticker} Equity");
    let fetcher = MockMasterFetcher {
        identity_raw: identity_raw_for(&security),
        ..Default::default()
    };
    let req = AddToBook {
        raw: ticker.to_string(),
        yellow_key: "Equity".into(),
        asset_class_id: class_id,
        label: label.into(),
        hints: Hints::default(),
    };
    match book::add(pool, &fetcher, &req, "test").await.unwrap() {
        AddOutcome::Added(entry) => entry,
        other => panic!("expected Added for {ticker}, got {other:?}"),
    }
}

/// The hit ledger must record what was actually dispatched to Bloomberg, not
/// the pre-flight gate estimate. `asset_class.corp_actions_capable` defaults
/// to true (migration 0011), so this fixture's lone book entry is priced by
/// `orchestrator::corp_actions_estimate` too: the gate estimate is 1 EOD hit
/// + 2 corp-action hits = 3. Before this task, `record_hits` charged that
/// whole gate estimate into the ledger even though the corp-action leg bills
/// itself separately at the wire seam (master_fetch.rs) -- a double count.
/// The ledger must show only the 1 hit this run's single security x single
/// field x single day actually dispatched.
#[tokio::test]
#[ignore = "requires postgres"]
async fn the_ledger_records_dispatched_hits_not_the_gate_estimate() {
    use getbloomdata_lib::error::AppResult;
    use getbloomdata_lib::fetch::{FetchOutcome, FetchRequest};
    use getbloomdata_lib::orchestrator::{self, DataFetcher, PipelineConfig};
    use getbloomdata_lib::{fields, registry, views};
    use std::path::Path;

    struct EmptyFetcher;
    impl DataFetcher for EmptyFetcher {
        async fn fetch(&self, _req: &FetchRequest, _audit: Option<&Path>)
            -> AppResult<FetchOutcome> {
            Ok(FetchOutcome::default())
        }
    }

    let pool = common::pool().await;
    let class = registry::create_asset_class(&pool, &uniq("OpsCls"), "t").await.unwrap();
    let field = fields::create_field(
        &pool, class.id, "PX_LAST", "Last", "numeric",
        None, None, "", false, None, None).await.unwrap();
    let entry = add_book_entry(&pool, class.id, "OpsA", &uniq("OPSA")).await;
    let view = views::create_view(&pool, &uniq("opsview"), "").await.unwrap();
    views::set_view_instruments(&pool, view.id, &[entry.instrument_id]).await.unwrap();
    views::set_view_fields(&pool, view.id, &[field.id]).await.unwrap();

    let cfg = PipelineConfig {
        data_dir: std::env::temp_dir(),
        python_path: "python".into(),
        script_path: "unused".into(),
        request_timeout_s: 5,
        soft_limit: 1_000_000,
    };
    let day = d("2026-08-17");
    let out = orchestrator::run_eod_with(
        &pool, &cfg, &EmptyFetcher, view.id, "manual", day, true)
        .await.unwrap();
    let run_id = match out {
        orchestrator::RunOutcome::Completed { run_id, .. } => run_id,
        other => panic!("expected Completed, got {other:?}"),
    };

    // The fixture must actually carry a corp-action leg for this test to
    // prove anything -- confirm the gate estimate is inflated by it.
    let estimated: i64 = sqlx::query_scalar("SELECT estimated_hits FROM run WHERE id = $1")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    assert!(estimated > 1,
            "fixture's corp_actions_capable class should inflate the gate estimate, got {estimated}");

    let hits: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(estimated_hits), 0)::bigint FROM hit_ledger WHERE run_id = $1")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    assert_eq!(hits, 1, "ledger must record exactly what was dispatched, not the gate estimate");
}
