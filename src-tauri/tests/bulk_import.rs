mod common;

use common::uniq;
use getbloomdata_lib::bulk::{self, sheet::file_sha256};
use getbloomdata_lib::error::AppResult;
use getbloomdata_lib::master_fetch::{parse_identity, Answered, HistIdRow, IdentityBlock, MasterFetcher};
use getbloomdata_lib::resolution::score::Candidate;
use sqlx::PgPool;
use std::path::Path;

/// Writes a hand-built sheet -- no `instrument_id` column -- with one row per
/// `(identifier, label)` pair, class "Equity" and yellow_key "Equity"
/// throughout. See the doc comment at its call site for why the import test
/// below needs this instead of `bulk::sheet::write_assets_sheet`.
fn write_hand_built_sheet(path: &Path, rows: &[(&str, &str)]) {
    let mut book = rust_xlsxwriter::Workbook::new();
    let s = book.add_worksheet();
    s.set_name(getbloomdata_lib::bulk::sheet::SHEET_NAME).unwrap();
    for (c, h) in ["label", "class", "identifier", "yellow_key"].iter().enumerate() {
        s.write_string(0, c as u16, *h).unwrap();
    }
    for (i, (identifier, label)) in rows.iter().enumerate() {
        let r = (i + 1) as u32;
        s.write_string(r, 0, *label).unwrap();
        s.write_string(r, 1, "Equity").unwrap();
        s.write_string(r, 2, *identifier).unwrap();
        s.write_string(r, 3, "Equity").unwrap();
    }
    book.save(path).unwrap();
}

/// Answers the ticker1-prefixed row with a clean identity and the
/// ticker2-prefixed row with two listings, so one row imports and the other
/// opens a review. `MockMasterFetcher` returns the same canned answer for
/// every call, which is not enough here -- mirrors `KeyedFetcher` in
/// tests/resolution.rs.
struct TwoRowFetcher {
    ticker1: String,
    ticker2: String,
    figi1: String,
}

impl MasterFetcher for TwoRowFetcher {
    async fn identity(&self, securities: &[String]) -> AppResult<Answered<Vec<IdentityBlock>>> {
        if securities.iter().any(|s| s.starts_with(&self.ticker1)) {
            let raw = serde_json::json!([{"securityData": [{
                "security": format!("{} US Equity", self.ticker1),
                "fieldExceptions": [], "sequenceNumber": 0,
                "fieldData": {
                    "ID_BB_GLOBAL": self.figi1, "EXCH_CODE": "US", "CRNCY": "USD",
                    "NAME": "IMPORT ONE INC", "LISTING_DATE": "2000-01-03"}}]}]);
            let parsed = parse_identity(&raw);
            return Ok(Answered { parsed, raw });
        }
        // ticker2 is not resolvable by reference; it falls to search.
        Ok(Answered { parsed: vec![], raw: serde_json::json!([]) })
    }

    async fn hist_ids(&self, _s: &str, _a: &str, _d: chrono::NaiveDate)
        -> AppResult<Vec<HistIdRow>>
    {
        Ok(vec![])
    }

    async fn instrument_list(&self, _q: &str, _yk: Option<&str>, _max: u32)
        -> AppResult<Answered<Vec<Candidate>>>
    {
        let parsed = vec![
            Candidate { security: format!("{} US Equity", self.ticker2),
                        description: "Import Two".into(),
                        exchange: Some("US".into()), country: None, currency: None,
                        asset_class: None, figi: None },
            Candidate { security: format!("{} LN Equity", self.ticker2),
                        description: "Import Two".into(),
                        exchange: Some("LN".into()), country: None, currency: None,
                        asset_class: None, figi: None },
        ];
        Ok(Answered { parsed, raw: serde_json::json!([]) })
    }
}

/// Shared 'Equity' class, reused (not `uniq()`-ed) via `ON CONFLICT DO
/// NOTHING`: a class name racing another test's identical insert is
/// harmless and idempotent, unlike the securities/labels below which must be
/// unique per Task 13 correction E.
async fn equity_class(pool: &PgPool) -> i64 {
    sqlx::query("INSERT INTO asset_class (name) VALUES ('Equity') ON CONFLICT (name) DO NOTHING")
        .execute(pool).await.unwrap();
    sqlx::query_scalar("SELECT id FROM asset_class WHERE name = 'Equity'")
        .fetch_one(pool).await.unwrap()
}

/// Spec §8: an imported row that resolves ambiguously creates a review row
/// instead of failing the import. A book of two hundred lines must not stop
/// dead because one of them is ambiguous.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_ambiguous_imported_row_opens_a_review_and_the_rest_still_import() {
    let pool = common::pool().await;
    equity_class(&pool).await;

    let ticker1 = uniq("ZIMP1");
    let ticker2 = uniq("ZIMP2");
    let fetcher = TwoRowFetcher {
        ticker1: ticker1.clone(),
        ticker2: ticker2.clone(),
        figi1: uniq("BBG000IMPORT1"),
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.xlsx");
    // Deliberately hand-built (no `instrument_id` header), not written through
    // `write_assets_sheet`: that always emits the id column, which under
    // guardrail 1 makes the sheet an authoritative full export -- every
    // existing active book_entry NOT named by id would then be proposed for
    // removal. In this shared, never-cleaned `bloom_test` database (Task 13
    // correction E) that is every row every other test has ever left behind,
    // which guardrail 2 then refuses outright. A hand-built sheet adding two
    // brand-new rows and saying nothing about the rest of the book is exactly
    // what guardrail 1 exists to make safe.
    write_hand_built_sheet(&path, &[
        (&format!("{ticker1} US"), "Import One"),
        (&ticker2, "Import Two"),
    ]);
    let hash = file_sha256(&path).unwrap();

    let result = bulk::apply_import_with(&pool, &fetcher, &path, &hash, &[], None)
        .await.unwrap();

    assert_eq!(result.added, 1, "the resolvable row imports");
    assert_eq!(result.reviews_opened, 1, "the ambiguous row waits for a human");

    // Scoped to this test's own securities/raw_input, never a global count or
    // before/after delta -- Task 13 correction E: the shared bloom_test
    // database races other tests running in parallel and is never cleaned up.
    let book: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM book_entry b
           JOIN instrument_alias a ON a.instrument_id = b.instrument_id
          WHERE a.id_type = 'bdp_security' AND a.value LIKE $1")
        .bind(format!("{ticker1}%"))
        .fetch_one(&pool).await.unwrap();
    assert_eq!(book, 1, "an ambiguous row must not quietly enter the book");

    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM resolution_review r
           JOIN resolution_decision d ON d.id = r.decision_id
          WHERE r.status = 'pending' AND d.raw_input = $1")
        .bind(&ticker2)
        .fetch_one(&pool).await.unwrap();
    assert_eq!(pending, 1);
}
