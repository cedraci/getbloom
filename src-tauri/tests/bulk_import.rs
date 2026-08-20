mod common;

use common::uniq;
use getbloomdata_lib::bulk::{
    self,
    sheet::{file_sha256, read_assets_sheet, write_assets_sheet, ExportRow},
};
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

    async fn corp_actions(&self, _securities: &[String])
        -> AppResult<Answered<Vec<getbloomdata_lib::fetch::SidecarBulkRows>>>
    {
        Ok(Answered { parsed: vec![], raw: serde_json::json!([]) })
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

/// Review-round-1, Critical finding: `apply_import_with`'s post-commit
/// rewrite replaces the user's whole file with a fresh export -- which, until
/// this fix, only ever wrote `book_entry` rows. A blank-id row that came back
/// `NeedsReview` never entered the book, so it was silently erased from the
/// file the user is looking at, with nothing on screen saying so. The
/// workbook is the migration tool; those rows may be the user's only record.
///
/// Uses a real, full export as the sheet's starting point (every row this
/// test did not add round-trips unchanged) rather than a hand-built sheet, so
/// this specifically exercises the id-bearing rewrite path
/// (`plan.has_id_column` true) that the previous test above deliberately
/// avoids. Reading the export back and re-exporting it verbatim before
/// appending two new rows means guardrail 1 proposes zero removals no matter
/// how large the shared `bloom_test` database has grown from other tests
/// (Task 13 correction E) -- the alternative, retiring every other active
/// entry to satisfy the reviewed-removal check, would be destructive to
/// whatever concurrent tests are relying on.
///
/// The gap between the export read and the apply below is a real, narrow race
/// against any other writer in this shared, parallel, never-cleaned database
/// -- including this file's own sibling test, observed in practice: a
/// concurrent `book::add` between the export and the apply makes a stale,
/// unreviewed removal appear and `apply_import_with` correctly refuses it (no
/// side effects: that check runs before anything is written). Retried a
/// bounded number of times from a fresh export rather than serialized against
/// every other test in the suite.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_row_that_opens_a_review_survives_the_post_apply_rewrite() {
    let pool = common::pool().await;
    equity_class(&pool).await;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.xlsx");

    let ticker_ok = uniq("ZREW1");
    let ticker_review = uniq("ZREW2");
    let fetcher = TwoRowFetcher {
        ticker1: ticker_ok.clone(),
        ticker2: ticker_review.clone(),
        figi1: uniq("BBG000REWRITE1"),
    };

    let mut result = None;
    for attempt in 0..10 {
        bulk::export_assets_xlsx(&pool, &path).await.unwrap();
        let existing = read_assets_sheet(&path).unwrap();
        assert!(existing.has_id_column, "a real export always carries the id column");

        let mut rows: Vec<ExportRow> = existing.rows.iter().map(|r| ExportRow {
            instrument_id: r.instrument_id.unwrap_or(0), label: r.label.clone(),
            class: r.class.clone(), identifier: r.identifier.clone(),
            yellow_key: r.yellow_key.clone(), active: r.active,
            security: String::new(), status: String::new(), views: r.views.clone(),
        }).collect();
        rows.push(ExportRow {
            instrument_id: 0, label: "Rewrite One".into(), class: "Equity".into(),
            identifier: format!("{ticker_ok} US"), yellow_key: "Equity".into(),
            active: true, security: String::new(), status: String::new(), views: vec![],
        });
        rows.push(ExportRow {
            instrument_id: 0, label: "Rewrite Two".into(), class: "Equity".into(),
            identifier: ticker_review.clone(), yellow_key: "Equity".into(),
            active: true, security: String::new(), status: String::new(), views: vec![],
        });
        write_assets_sheet(&path, &rows, &existing.view_columns, &["Equity".into()]).unwrap();
        let hash = file_sha256(&path).unwrap();

        match bulk::apply_import_with(&pool, &fetcher, &path, &hash, &[], None).await {
            Ok(r) => { result = Some(r); break; }
            Err(e) => eprintln!(
                "attempt {attempt}: retrying after a concurrent writer raced the export: {e}"),
        }
    }
    let result = result.expect("apply_import_with should succeed within 10 attempts");

    assert_eq!(result.added, 1, "the resolvable row imports");
    assert_eq!(result.reviews_opened, 1, "the ambiguous row waits for a human");
    assert!(result.workbook_refreshed, "an id-bearing sheet is always rewritten on success");

    // Reread the file the apply just wrote. The row that opened a review
    // never became a book entry, so it carries no id -- but it must still be
    // there, not silently dropped by the rewrite.
    let after = read_assets_sheet(&path).unwrap();
    let survivor = after.rows.iter().find(|r| r.identifier == ticker_review)
        .unwrap_or_else(|| panic!(
            "the row that opened a review must survive the post-apply rewrite, \
             got identifiers {:?}",
            after.rows.iter().map(|r| &r.identifier).collect::<Vec<_>>()));
    assert_eq!(survivor.instrument_id, None,
               "a row that never became a book entry must still show a blank id");
}
