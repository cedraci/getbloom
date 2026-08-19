//! Bulk instrument management through an Excel round trip.
//!
//! Three files with hard boundaries, because that boundary is what makes the
//! interesting logic testable without Postgres or Excel:
//!   sheet.rs  -- files only, never the database
//!   diff.rs   -- pure functions, neither files nor the database
//!   mod.rs    -- the only place that does both
//!
//! The workbook's subject is `book_entry` + resolution, not the removed
//! `asset` table: a row that resolves ambiguously opens a review instead of
//! failing the whole import (spec §8).

pub mod diff;
pub mod sheet;

use crate::book::{self, AddOutcome, AddToBook};
use crate::deletion::{purge_book_entry_tx, DeleteMode};
use crate::error::{AppError, AppResult};
use crate::instrument::store;
use crate::master_fetch::{BlpapiMasterFetcher, MasterFetcher};
use crate::orchestrator::PipelineConfig;
use crate::resolution::score::Hints;
use diff::{DbInstrument, ImportPlan};
use serde::{Deserialize, Serialize};
use sheet::ExportRow;
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportResult {
    pub added: i64,
    pub edited: i64,
    pub retired: i64,
    pub reactivated: i64,
    /// Count of INSTRUMENTS whose view membership changed, not of memberships
    /// -- an instrument that gains two views and loses one still counts once
    /// here.
    pub membership_assets_updated: i64,
    pub removed: i64,
    /// A row that resolved ambiguously and opened a review instead of binding
    /// (spec §8). Not a failure: a book of two hundred lines must not stop
    /// dead because one line is ambiguous.
    pub reviews_opened: usize,
    /// Whether the post-commit re-export of the workbook (see `apply_import`)
    /// actually landed on disk. `false` means the committed changes are real
    /// but the file the user is looking at is now stale and must be
    /// re-exported before the next preview/apply.
    pub workbook_refreshed: bool,
}

/// The book sheet shows `identifier` and `yellow_key` as two columns, but the
/// only thing actually stored is the derived `bdp_security` alias -- one
/// instrument wears several security strings over its life, never a fixed
/// identifier/key pair. Splitting the security valid today back into the two
/// display columns is the (best-effort) inverse of
/// `resolution::normalize::build_security`.
fn split_security(security: &str) -> (String, String) {
    match security.rsplit_once(char::is_whitespace) {
        Some((head, tail)) => {
            let identifier = head.strip_prefix("/isin/").unwrap_or(head);
            (identifier.to_string(), tail.to_string())
        }
        None => (security.to_string(), String::new()),
    }
}

/// Every book entry, flattened to the names the sheet and the differ speak
/// in. Inactive entries are included: the sheet is the whole book, and an
/// `active` of "no" is how the user sees a retired name.
pub async fn load_db_instruments(pool: &PgPool) -> AppResult<Vec<DbInstrument>> {
    let rows: Vec<(i64, String, String, bool)> = sqlx::query_as(
        "SELECT b.instrument_id, b.label, c.name, b.active
         FROM book_entry b JOIN asset_class c ON c.id = b.asset_class_id
         ORDER BY b.label")
        .fetch_all(pool).await?;

    let memberships: Vec<(i64, String)> = sqlx::query_as(
        "SELECT vi.instrument_id, v.name
         FROM view_instrument vi JOIN view v ON v.id = vi.view_id")
        .fetch_all(pool).await?;
    let mut by_instrument: HashMap<i64, Vec<String>> = HashMap::new();
    for (iid, name) in memberships {
        by_instrument.entry(iid).or_default().push(name);
    }

    let today = chrono::Local::now().date_naive();
    let mut out = Vec::with_capacity(rows.len());
    for (instrument_id, label, class, active) in rows {
        // A blank security -- no alias valid today -- reads as blank
        // identifier/yellow_key too, rather than failing the whole export.
        let security = store::current_security(pool, instrument_id, today).await?
            .unwrap_or_default();
        let (identifier, yellow_key) = split_security(&security);
        out.push(DbInstrument {
            instrument_id, label, class, identifier, yellow_key, active, security,
            views: by_instrument.remove(&instrument_id).unwrap_or_default(),
        });
    }
    Ok(out)
}

async fn view_names(pool: &PgPool) -> AppResult<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM view ORDER BY name")
        .fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

async fn class_names(pool: &PgPool) -> AppResult<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM asset_class ORDER BY name")
        .fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

pub async fn export_assets_xlsx(pool: &PgPool, path: &Path) -> AppResult<()> {
    let instruments = load_db_instruments(pool).await?;
    let views = view_names(pool).await?;
    let classes = class_names(pool).await?;
    let rows: Vec<ExportRow> = instruments.into_iter().map(|a| {
        // `status` is written for the reader only, never read back: every row
        // here already lives in book_entry, so "unresolved" means only that
        // its security has no alias valid today (e.g. delisted), not that it
        // is awaiting a review -- a raw input still `pending` in
        // resolution_review never made it into book_entry in the first place.
        let status = if a.security.is_empty() { "unresolved" } else { "resolved" }.to_string();
        ExportRow {
            instrument_id: a.instrument_id, label: a.label, class: a.class,
            identifier: a.identifier, yellow_key: a.yellow_key, active: a.active,
            security: a.security, status, views: a.views,
        }
    }).collect();
    sheet::write_assets_sheet(path, &rows, &views, &classes)
}

async fn plan_for(pool: &PgPool, path: &Path) -> AppResult<ImportPlan> {
    let data = sheet::read_assets_sheet(path)?;
    let hash = sheet::file_sha256(path)?;
    let db = load_db_instruments(pool).await?;
    let views = view_names(pool).await?;
    let classes = class_names(pool).await?;
    Ok(diff::diff(&data, &db, &classes, &views, &hash))
}

pub async fn preview_import(pool: &PgPool, path: &Path) -> AppResult<ImportPlan> {
    plan_for(pool, path).await
}

/// Re-reads and re-diffs the file, then applies everything reviewable or
/// nothing, and opens a review for anything that is not.
///
/// The hash check is the point of the two phases: a plan the user reviewed can
/// never be applied against a file that changed underneath it. The re-diff
/// matters just as much -- the database may have moved on even when the file
/// has not. Every fresh removal must be named in `removal_modes`, or the
/// whole call is refused: nothing is applied against a removal the caller
/// never reviewed. `removal_modes` also may not name the same id twice with
/// conflicting intent -- see the duplicate-key check below.
///
/// The reverse direction is deliberately NOT checked: an extra, stale key in
/// `removal_modes` naming an id that is no longer a removal in the fresh plan
/// is harmless and ignored. The sheet itself cannot have changed without
/// invalidating the file hash above, so the only way an id can drop out of
/// the removal set between preview and apply is another writer deleting that
/// instrument from the database first -- and a mode for an instrument that is
/// already gone simply has nothing left to act on.
///
/// Added rows are resolved through `book::add` -- the same path the UI's "add
/// to book" uses -- one call per row, each its own atomic unit. This is
/// deliberately NOT part of the transaction below: an ambiguous row must open
/// a review and tally into `ImportResult.reviews_opened`, not roll back a
/// book of two hundred lines because one line could not be bound (spec §8).
/// A row Bloomberg has nothing for (`AddOutcome::NotFound`) is skipped the
/// same way -- not an ambiguity, and not a reason to abort either; the row
/// stays on disk in the sheet for the user to fix or remove on the next round
/// trip.
///
/// Once the transactional part commits, the workbook is re-exported over
/// `path` best-effort -- see the comment at that call site for why a failed
/// refresh still returns `Ok`.
pub async fn apply_import_with<F: MasterFetcher>(
    pool: &PgPool,
    fetcher: &F,
    path: &Path,
    file_hash: &str,
    removal_modes: &[(i64, DeleteMode)],
    confirmed_removal_count: Option<i64>,
) -> AppResult<ImportResult> {
    let actual = sheet::file_sha256(path)?;
    if actual != file_hash {
        return Err(AppError::ImportRejected {
            reason: "the file changed since it was previewed; preview it again".into(),
        });
    }
    let plan = plan_for(pool, path).await?;
    // `plan_for` re-reads and re-hashes the file itself. Checking its hash too
    // closes the gap between the read above and this one -- a file that
    // changed in that window must not be applied either -- and the second
    // read already paid for the comparison, so this costs nothing extra.
    if plan.file_hash != file_hash {
        return Err(AppError::ImportRejected {
            reason: "the file changed since it was previewed; preview it again".into(),
        });
    }
    if !plan.invalid_rows.is_empty() {
        return Err(AppError::ImportRejected {
            reason: format!("{} invalid row(s); nothing was applied", plan.invalid_rows.len()),
        });
    }
    if plan.requires_typed_confirmation
        && confirmed_removal_count != Some(plan.removals.len() as i64)
    {
        return Err(AppError::ImportRejected {
            reason: format!(
                "this removes {} of {} active instruments; confirm the count to proceed",
                plan.removals.len(), plan.active_asset_count),
        });
    }

    // Built by hand rather than `.collect()`-ed into a HashMap: collecting
    // would let a later duplicate `(id, mode)` pair silently overwrite an
    // earlier one for the same id, so slice order alone would decide whether
    // a given instrument is retired or purged. A caller that names the same
    // instrument twice with conflicting intent must be told, not guessed at.
    let mut modes: HashMap<i64, DeleteMode> = HashMap::with_capacity(removal_modes.len());
    for &(id, mode) in removal_modes {
        if let Some(prev) = modes.insert(id, mode) {
            return Err(AppError::ImportRejected {
                reason: format!(
                    "removal mode for instrument #{id} was given twice ({prev:?} then {mode:?}); \
                     decide once and resubmit"),
            });
        }
    }

    // Every removal about to be applied must be one the caller actually
    // reviewed. Without this, an instrument that vanished from the sheet only
    // because another writer changed the book between preview and apply --
    // never something the user looked at -- would silently fall back to
    // Retire below. A stale plan must be refused outright, not partially
    // honoured with a guessed mode.
    let unreviewed = plan.removals.iter().filter(|r| !modes.contains_key(&r.id)).count();
    if unreviewed > 0 {
        return Err(AppError::ImportRejected {
            reason: format!(
                "{unreviewed} removal(s) were not part of the reviewed plan; \
                 the book changed since this sheet was previewed -- preview it again"),
        });
    }

    let classes: HashMap<String, i64> = {
        let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM asset_class")
            .fetch_all(pool).await?;
        rows.into_iter().map(|(id, n)| (n, id)).collect()
    };
    let views: HashMap<String, i64> = {
        let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM view")
            .fetch_all(pool).await?;
        rows.into_iter().map(|(id, n)| (n, id)).collect()
    };
    let class_id = |name: &str| -> AppResult<i64> {
        classes.get(name).copied()
            .ok_or_else(|| AppError::ImportRejected { reason: format!("no class '{name}'") })
    };
    let view_id = |name: &str| -> AppResult<i64> {
        views.get(name).copied()
            .ok_or_else(|| AppError::ImportRejected { reason: format!("no view '{name}'") })
    };

    let mut res = ImportResult::default();

    // Adds first, and outside the transaction below -- see the doc comment on
    // this function for why. Each row costs Bloomberg calls only through
    // `book::add`/resolution, which already never calls out for a row that
    // resolves locally for free.
    for a in &plan.adds {
        let req = AddToBook {
            raw: a.identifier.clone(),
            yellow_key: a.yellow_key.clone(),
            asset_class_id: class_id(&a.class)?,
            label: a.label.clone(),
            hints: Hints::default(),
        };
        match book::add(pool, fetcher, &req, "bulk_import").await? {
            AddOutcome::Added(entry) => {
                if !a.active {
                    book::set_active(pool, entry.instrument_id, false).await?;
                }
                for v in &a.views {
                    sqlx::query(
                        "INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)
                         ON CONFLICT DO NOTHING")
                        .bind(view_id(v)?).bind(entry.instrument_id)
                        .execute(pool).await?;
                }
                res.added += 1;
            }
            AddOutcome::NeedsReview { .. } => {
                res.reviews_opened += 1;
            }
            AddOutcome::NotFound => {
                // Nothing to bind and nothing to review; see the doc comment
                // on this function.
            }
        }
    }

    let mut tx = pool.begin().await?;

    for e in &plan.edits {
        // Only `label` and `class` are book_entry's own columns; identity
        // (identifier/yellow_key/security) is immutable in place -- see
        // diff::diff's rejection of an identifier or yellow-key change.
        sqlx::query("UPDATE book_entry SET asset_class_id = $2, label = $3 WHERE instrument_id = $1")
            .bind(e.id).bind(class_id(&e.class)?).bind(&e.label)
            .execute(&mut *tx).await?;
        res.edited += 1;
    }

    for m in &plan.membership_changes {
        for v in &m.added {
            sqlx::query(
                "INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)
                 ON CONFLICT DO NOTHING")
                .bind(view_id(v)?).bind(m.id).execute(&mut *tx).await?;
        }
        for v in &m.removed {
            sqlx::query("DELETE FROM view_instrument WHERE view_id = $1 AND instrument_id = $2")
                .bind(view_id(v)?).bind(m.id).execute(&mut *tx).await?;
        }
        res.membership_assets_updated += 1;
    }

    for r in &plan.retires {
        sqlx::query("UPDATE book_entry SET active = false WHERE instrument_id = $1")
            .bind(r.id).execute(&mut *tx).await?;
        res.retired += 1;
    }
    for r in &plan.reactivations {
        sqlx::query("UPDATE book_entry SET active = true WHERE instrument_id = $1")
            .bind(r.id).execute(&mut *tx).await?;
        res.reactivated += 1;
    }

    // Removals last, so a purge never pulls the rug from under an edit above.
    // The check above guarantees every id here is a key in `modes`; the
    // fallback to Retire is defensive only and should never actually fire.
    for r in &plan.removals {
        match modes.get(&r.id).copied().unwrap_or(DeleteMode::Retire) {
            DeleteMode::Retire => {
                sqlx::query("UPDATE book_entry SET active = false WHERE instrument_id = $1")
                    .bind(r.id).execute(&mut *tx).await?;
            }
            DeleteMode::Purge => purge_book_entry_tx(&mut tx, r.id).await?,
        }
        res.removed += 1;
    }

    tx.commit().await?;

    // Re-export over the same path so the file on disk matches what was just
    // committed -- but ONLY when the sheet carries an `instrument_id` column,
    // i.e. only for a file this app produced by Export. Without this
    // narrowing, a hand-built or pasted sheet -- no `instrument_id` column,
    // therefore guardrail-1-safe and never able to propose a removal --
    // would get replaced wholesale by a full book export after every apply,
    // destroying whatever the user built by hand (their own column order,
    // their own extra columns) even though nothing about their file demanded
    // it.
    //
    // A file with an `instrument_id` column DOES need the rewrite: a
    // blank-id add row keeps its blank id on disk after the database has
    // assigned it a real one, so re-previewing the same file would read the
    // now-persisted instrument as both an invalid duplicate claim on its own
    // security and a removal (its id never appears as "present" in a sheet
    // where its row still has no id). Every other kind of change already
    // leaves the file matching the database post-apply -- edits, membership
    // marks, retires and reactivations all come from data the file already
    // held -- so adds are the only reason this step is required for an
    // id-bearing file.
    //
    // Skipping the rewrite for a hand-built sheet is safe: such a sheet has no
    // ids to write back, cannot propose a removal under guardrail 1, and its
    // next preview fails loudly with the existing "already belongs to
    // instrument #N ... export it again" invalid-row message rather than
    // destructively.
    //
    // When it does run, this MUST be best-effort. The transaction above
    // already committed (and every add already committed for real through
    // `book::add`, above): the change is real and the caller must be told it
    // succeeded no matter what happens next. The file can be locked by the
    // user's own open copy of it in Excel (a sharing violation, `os error 5`
    // on Windows) or by anything else transient; none of that may turn a
    // landed write into a reported failure, which would both lie to the
    // caller and leave a retry stuck -- a removal-only retry would keep
    // re-committing an empty transaction and keep failing on the same locked
    // file forever, and an add-bearing retry would be flatly rejected as
    // invalid (the blank-id row now collides with the instrument it already
    // created), reading exactly like "you pasted a duplicate, delete this
    // row" when the real fix is "export again".
    res.workbook_refreshed = if plan.has_id_column {
        export_assets_xlsx(pool, path).await.is_ok()
    } else {
        false
    };

    Ok(res)
}

/// Thin wrapper over `apply_import_with` for real callers: constructs the
/// live Bloomberg fetcher so a caller that already has a `PipelineConfig`
/// (every Tauri command does -- see `commands::pipeline_cfg`) does not have
/// to know `apply_import_with` exists. Tests that need to control resolution
/// (an ambiguous row, a mocked identity) call `apply_import_with` directly
/// with a mock `MasterFetcher`.
pub async fn apply_import(
    pool: &PgPool,
    cfg: &PipelineConfig,
    path: &Path,
    file_hash: &str,
    removal_modes: &[(i64, DeleteMode)],
    confirmed_removal_count: Option<i64>,
) -> AppResult<ImportResult> {
    let fetcher = BlpapiMasterFetcher { cfg };
    apply_import_with(pool, &fetcher, path, file_hash, removal_modes, confirmed_removal_count).await
}
