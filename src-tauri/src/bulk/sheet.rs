//! Reading and writing the book workbook. No database access lives here.

use crate::error::{AppError, AppResult};
use calamine::{Data, Reader, Xlsx};
use rust_xlsxwriter::{DataValidation, Format, Workbook};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const SHEET_NAME: &str = "Book";
pub const FIXED_HEADERS: [&str; 8] = [
    "instrument_id", "label", "class", "identifier", "yellow_key", "active",
    "security", "status",
];

/// One instrument as it appears in the exported workbook. `security` and
/// `status` are written for the reader's benefit only -- import ignores both:
/// `security` is derived from the alias valid today, and `status` reflects the
/// review queue.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportRow {
    pub instrument_id: i64,
    pub label: String,
    pub class: String,
    pub identifier: String,
    pub yellow_key: String,
    pub active: bool,
    pub security: String,
    pub status: String,
    /// Names of the views this instrument belongs to.
    pub views: Vec<String>,
}

fn xlsx_err(e: rust_xlsxwriter::XlsxError) -> AppError {
    AppError::Validation(format!("spreadsheet error: {e}"))
}

pub fn write_assets_sheet(
    path: &Path,
    rows: &[ExportRow],
    view_names: &[String],
    class_names: &[String],
) -> AppResult<()> {
    let mut book = Workbook::new();
    let sheet = book.add_worksheet();
    sheet.set_name(SHEET_NAME).map_err(xlsx_err)?;

    let header = Format::new().set_bold();
    let readonly = Format::new().set_background_color(0xEEEEEE);

    for (c, h) in FIXED_HEADERS.iter().enumerate() {
        sheet.write_string_with_format(0, c as u16, *h, &header).map_err(xlsx_err)?;
    }
    for (i, v) in view_names.iter().enumerate() {
        let c = (FIXED_HEADERS.len() + i) as u16;
        sheet.write_string_with_format(0, c, v, &header).map_err(xlsx_err)?;
    }
    // The header stays put while scrolling a few hundred rows.
    sheet.set_freeze_panes(1, 0).map_err(xlsx_err)?;

    for (i, r) in rows.iter().enumerate() {
        let row = (i + 1) as u32;
        // A blank instrument_id is how the sheet says "new instrument". Writing
        // 0 here would read back as instrument_id 0 and be rejected as an id
        // not in the database, so a row assembled for an add must leave the
        // cell empty.
        if r.instrument_id > 0 {
            sheet.write_number_with_format(row, 0, r.instrument_id as f64, &readonly)
                .map_err(xlsx_err)?;
        } else {
            sheet.write_blank(row, 0, &readonly).map_err(xlsx_err)?;
        }
        sheet.write_string(row, 1, &r.label).map_err(xlsx_err)?;
        sheet.write_string(row, 2, &r.class).map_err(xlsx_err)?;
        sheet.write_string(row, 3, &r.identifier).map_err(xlsx_err)?;
        sheet.write_string(row, 4, &r.yellow_key).map_err(xlsx_err)?;
        sheet.write_string(row, 5, if r.active { "yes" } else { "no" }).map_err(xlsx_err)?;
        sheet.write_string_with_format(row, 6, &r.security, &readonly).map_err(xlsx_err)?;
        sheet.write_string_with_format(row, 7, &r.status, &readonly).map_err(xlsx_err)?;
        for (j, v) in view_names.iter().enumerate() {
            let c = (FIXED_HEADERS.len() + j) as u16;
            let mark = if r.views.iter().any(|x| x == v) { "x" } else { "" };
            sheet.write_string(row, c, mark).map_err(xlsx_err)?;
        }
    }

    // Dropdowns turn the most typo-prone columns into pick lists.
    let last = rows.len().max(1) as u32;
    let classes: Vec<&str> = class_names.iter().map(String::as_str).collect();
    let dv_class = DataValidation::new().allow_list_strings(&classes).map_err(xlsx_err)?;
    sheet.add_data_validation(1, 2, last, 2, &dv_class).map_err(xlsx_err)?;
    let dv_active = DataValidation::new()
        .allow_list_strings(&["yes", "no"]).map_err(xlsx_err)?;
    sheet.add_data_validation(1, 5, last, 5, &dv_active).map_err(xlsx_err)?;
    // `yellow_key` deliberately has no dropdown: the set is open-ended
    // (Equity, Corp, Index, Curncy, Comdty, Govt, ...) and constraining it
    // would block a legitimate key nobody thought to list.
    // `identifier` deliberately has no dropdown either: id_kind is gone,
    // `detect_id_kind` reads the shape of whatever the user typed.

    // `Workbook::save` truncates its target (`File::create`, then streams the
    // zip) before writing a single byte of sheet content, so a failure
    // partway through -- disk full, a network share dropping, the process
    // being killed -- would otherwise leave the user's workbook a corrupt,
    // truncated zip. Writing beside the target and renaming over it on
    // success means the destination is only ever replaced by a complete
    // file. The temp file must live in the SAME directory as `path`: a
    // rename across volumes is not atomic (and can silently degrade to a
    // copy+delete, reopening the exact same window this is meant to close),
    // and `%TEMP%`/`/tmp` may well be on a different drive or filesystem
    // than the target.
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name()
        .ok_or_else(|| AppError::Validation(format!("not a file path: {}", path.display())))?
        .to_string_lossy();
    let tmp_path = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));

    if let Err(e) = book.save(&tmp_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(xlsx_err(e));
    }
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(AppError::Io(e));
    }
    Ok(())
}

pub fn file_sha256(path: &Path) -> AppResult<String> {
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}

/// One row as the user left it. Nothing here is validated or resolved: that is
/// the differ's job, so that validation can be unit-tested without a file.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetRow {
    /// 1-based spreadsheet row number, so error messages match what Excel shows.
    pub row_number: u32,
    pub instrument_id: Option<i64>,
    pub label: String,
    pub class: String,
    pub identifier: String,
    pub yellow_key: String,
    pub active: bool,
    pub views: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SheetData {
    /// False for a hand-built or pasted list. Guardrail 1: such a file can
    /// never propose a removal, because the absence of a row means nothing
    /// when the file was never a full export in the first place.
    pub has_id_column: bool,
    pub view_columns: Vec<String>,
    pub rows: Vec<SheetRow>,
}

fn cell_text(row: &[Data], idx: Option<usize>) -> String {
    match idx.and_then(|i| row.get(i)) {
        None | Some(Data::Empty) => String::new(),
        Some(Data::Float(f)) => {
            // Excel stores every number as a float; an id typed as 12 arrives
            // as 12.0 and must not become the string "12.0".
            if f.fract() == 0.0 { format!("{}", *f as i64) } else { f.to_string() }
        }
        Some(Data::Int(i)) => i.to_string(),
        Some(Data::Bool(b)) => (if *b { "yes" } else { "no" }).to_string(),
        Some(other) => other.to_string(),
    }
    .trim()
    .to_string()
}

pub fn read_assets_sheet(path: &Path) -> AppResult<SheetData> {
    let mut book: Xlsx<_> = calamine::open_workbook(path)
        .map_err(|e| AppError::Validation(format!("cannot open {}: {e}", path.display())))?;
    let range = book.worksheet_range(SHEET_NAME).map_err(|e| {
        AppError::Validation(format!("workbook has no sheet named '{SHEET_NAME}': {e}"))
    })?;

    let mut rows_iter = range.rows();
    let header_raw: Vec<String> = rows_iter
        .next()
        .ok_or_else(|| AppError::Validation("sheet is empty".into()))?
        .iter()
        .map(|c| c.to_string().trim().to_string())
        .collect();
    let header: Vec<String> = header_raw.iter().map(|h| h.to_lowercase()).collect();

    let col = |name: &str| header.iter().position(|h| h == name);
    let (c_id, c_label, c_class, c_identifier, c_key, c_active) = (
        col("instrument_id"), col("label"), col("class"),
        col("identifier"), col("yellow_key"), col("active"),
    );
    if c_label.is_none() {
        return Err(AppError::Validation("sheet has no 'label' column".into()));
    }

    // Anything that is not a known fixed header is a view column. `security`
    // and `status` are fixed headers and are read but discarded -- import
    // recomputes them. View names keep their original case, because they have
    // to match `view.name`.
    let (view_indices, view_columns): (Vec<usize>, Vec<String>) = header_raw
        .iter()
        .enumerate()
        .filter(|(_, h)| !h.is_empty() && !FIXED_HEADERS.contains(&h.to_lowercase().as_str()))
        .map(|(i, h)| (i, h.clone()))
        .unzip();

    let mut rows = Vec::new();
    for (offset, r) in rows_iter.enumerate() {
        let row_number = (offset + 2) as u32; // header is row 1
        if r.iter().all(|c| matches!(c, Data::Empty)) {
            continue;
        }
        let id_text = cell_text(r, c_id);
        let instrument_id = if id_text.is_empty() {
            None
        } else {
            Some(id_text.parse::<i64>().map_err(|_| {
                AppError::Validation(format!(
                    "row {row_number}: instrument_id '{id_text}' is not a number"))
            })?)
        };
        let active_text = cell_text(r, c_active).to_lowercase();
        let active = match active_text.as_str() {
            "no" | "n" | "false" | "0" | "non" => false,
            // Empty/absent, a recognized truthy token, or anything unrecognized
            // (a typo, a stray character) all resolve to active. A mistyped
            // cell that keeps collecting is visible and recoverable; one that
            // silently stops collecting is neither.
            _ => true,
        };
        let views = view_indices
            .iter()
            .zip(view_columns.iter())
            .filter(|(i, _)| !cell_text(r, Some(**i)).is_empty())
            .map(|(_, name)| name.clone())
            .collect();

        rows.push(SheetRow {
            row_number,
            instrument_id,
            label: cell_text(r, c_label),
            class: cell_text(r, c_class),
            identifier: cell_text(r, c_identifier),
            yellow_key: cell_text(r, c_key),
            active,
            views,
        });
    }

    Ok(SheetData { has_id_column: c_id.is_some(), view_columns, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<ExportRow> {
        vec![ExportRow {
            instrument_id: 7,
            label: "Apple".into(),
            class: "Equity".into(),
            identifier: "AAPL US".into(),
            yellow_key: "Equity".into(),
            active: true,
            security: "AAPL US Equity".into(),
            status: "resolved".into(),
            views: vec!["Daily".into()],
        }]
    }

    /// Writes headers only (no data rows) -- enough to exercise
    /// `has_id_column` without needing a full, valid row.
    fn write_minimal_sheet(path: &Path, headers: &[&str]) {
        let mut book = Workbook::new();
        let s = book.add_worksheet();
        s.set_name(SHEET_NAME).unwrap();
        for (c, h) in headers.iter().enumerate() {
            s.write_string(0, c as u16, *h).unwrap();
        }
        book.save(path).unwrap();
    }

    #[test]
    fn the_header_names_instruments_not_assets() {
        assert_eq!(FIXED_HEADERS,
                   ["instrument_id", "label", "class", "identifier", "yellow_key",
                    "active", "security", "status"]);
    }

    /// The id column is the guardrail from the 2026-08-18 work: a file without
    /// it was never a full export, so the absence of a row means nothing and no
    /// removal may be proposed. Renaming the column must not lose that.
    #[test]
    fn a_sheet_without_instrument_id_cannot_propose_removals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hand_built.xlsx");
        write_minimal_sheet(&path, &["label", "class", "identifier", "yellow_key"]);
        let data = read_assets_sheet(&path).unwrap();
        assert!(!data.has_id_column);
    }

    /// One column, not two. id_kind is gone: detect_id_kind reads the shape of
    /// the identifier, and a user who typed an ISIN into a "ticker" column was
    /// only ever telling us something we could see for ourselves.
    #[test]
    fn one_identifier_column_replaces_ticker_and_isin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        write_assets_sheet(&path, &[ExportRow {
            instrument_id: 7, label: "Apple".into(), class: "Equity".into(),
            identifier: "AAPL US".into(), yellow_key: "Equity".into(),
            active: true, security: "AAPL US Equity".into(),
            status: "resolved".into(), views: vec![] }], &[], &["Equity".into()])
            .unwrap();
        let data = read_assets_sheet(&path).unwrap();
        assert_eq!(data.rows[0].identifier, "AAPL US");
    }

    #[test]
    fn writes_a_file_with_the_expected_header_and_one_column_per_view() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("assets.xlsx");
        let views = vec!["Daily".to_string(), "Weekly".to_string()];
        write_assets_sheet(&path, &sample(), &views, &["Equity".to_string()]).unwrap();
        assert!(path.exists());
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
    }

    #[test]
    fn a_zero_id_is_written_as_a_blank_cell() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.xlsx");
        let mut rows = sample();
        rows[0].instrument_id = 0; // an instrument that does not exist yet
        write_assets_sheet(&path, &rows, &[], &["Equity".to_string()]).unwrap();
        // Proven properly in the reader task; here it is enough that the file
        // writes without turning 0 into a number the reader would parse.
        assert!(path.exists());
    }

    #[test]
    fn a_successful_write_replaces_an_existing_target_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("assets.xlsx");
        // Simulate re-exporting over the user's existing workbook.
        std::fs::write(&path, b"OLD CONTENT, NOT A REAL XLSX").unwrap();

        write_assets_sheet(&path, &sample(), &["Daily".to_string()], &["Equity".to_string()])
            .unwrap();

        let data = read_assets_sheet(&path).unwrap();
        assert_eq!(data.rows.len(), 1);
        assert_eq!(data.rows[0].label, "Apple");

        // Exactly the target file, no `.assets.xlsx.tmp-<pid>` left behind.
        let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["assets.xlsx".to_string()],
                   "a successful write must leave exactly the target, no temp leftovers");
    }

    #[test]
    fn a_failed_temp_write_never_touches_an_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("assets.xlsx");

        // Seed a target file with recognizable bytes, standing in for the
        // user's existing workbook -- it must survive byte-for-byte.
        std::fs::write(&path, b"ORIGINAL BYTES, MUST SURVIVE").unwrap();

        // Pre-occupy the exact temp path `write_assets_sheet` will try to
        // create, with a DIRECTORY of that name. `Workbook::save`'s internal
        // file creation cannot succeed against a path that is already a
        // directory, so this forces the temp-write step to fail before the
        // real target is ever opened -- without a lock, a full disk, or a
        // killed process, none of which are reproducible deterministically.
        let file_name = path.file_name().unwrap().to_string_lossy().to_string();
        let tmp_path = dir.path().join(format!(".{file_name}.tmp-{}", std::process::id()));
        std::fs::create_dir(&tmp_path).unwrap();

        let err = write_assets_sheet(&path, &sample(), &[], &["Equity".to_string()])
            .unwrap_err();
        assert!(matches!(err, AppError::Validation(_)), "got {err:?}");

        let survived = std::fs::read(&path).unwrap();
        assert_eq!(survived, b"ORIGINAL BYTES, MUST SURVIVE",
                   "a failed temp write must never touch the existing target");
    }

    #[test]
    fn hashing_the_same_bytes_twice_gives_the_same_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.bin");
        std::fs::write(&path, b"hello").unwrap();
        let a = file_sha256(&path).unwrap();
        let b = file_sha256(&path).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "sha-256 hex is 64 characters");
        std::fs::write(&path, b"hello!").unwrap();
        assert_ne!(a, file_sha256(&path).unwrap());
    }

    #[test]
    fn round_trips_a_written_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("assets.xlsx");
        let views = vec!["Daily".to_string(), "Weekly".to_string()];
        write_assets_sheet(&path, &sample(), &views, &["Equity".to_string()]).unwrap();

        let data = read_assets_sheet(&path).unwrap();
        assert!(data.has_id_column);
        assert_eq!(data.view_columns, views);
        assert_eq!(data.rows.len(), 1);
        let r = &data.rows[0];
        assert_eq!(r.row_number, 2, "spreadsheet rows are 1-based and row 1 is the header");
        assert_eq!(r.instrument_id, Some(7));
        assert_eq!(r.label, "Apple");
        assert_eq!(r.class, "Equity");
        assert_eq!(r.identifier, "AAPL US");
        assert_eq!(r.yellow_key, "Equity");
        assert!(r.active);
        assert_eq!(r.views, vec!["Daily".to_string()]);
    }

    #[test]
    fn a_blank_id_cell_reads_back_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.xlsx");
        let mut rows = sample();
        rows[0].instrument_id = 0;
        write_assets_sheet(&path, &rows, &[], &["Equity".to_string()]).unwrap();
        let data = read_assets_sheet(&path).unwrap();
        assert!(data.has_id_column, "the column exists even when its cells are blank");
        assert_eq!(data.rows[0].instrument_id, None, "a blank id means 'add this instrument'");
    }

    #[test]
    fn a_sheet_without_an_id_column_is_flagged() {
        use rust_xlsxwriter::Workbook;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pasted.xlsx");
        let mut book = Workbook::new();
        let s = book.add_worksheet();
        s.set_name(SHEET_NAME).unwrap();
        for (c, h) in ["label", "class", "identifier", "yellow_key"].iter().enumerate() {
            s.write_string(0, c as u16, *h).unwrap();
        }
        s.write_string(1, 0, "Microsoft").unwrap();
        s.write_string(1, 1, "Equity").unwrap();
        s.write_string(1, 2, "MSFT US").unwrap();
        s.write_string(1, 3, "Equity").unwrap();
        book.save(&path).unwrap();

        let data = read_assets_sheet(&path).unwrap();
        assert!(!data.has_id_column);
        assert_eq!(data.rows.len(), 1);
        assert_eq!(data.rows[0].instrument_id, None);
        assert_eq!(data.rows[0].label, "Microsoft");
        assert!(data.rows[0].active, "a sheet with no `active` column means all active");
    }

    #[test]
    fn blank_rows_are_skipped_not_read_as_empty_assets() {
        use rust_xlsxwriter::Workbook;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gappy.xlsx");
        let mut book = Workbook::new();
        let s = book.add_worksheet();
        s.set_name(SHEET_NAME).unwrap();
        for (c, h) in FIXED_HEADERS.iter().enumerate() {
            s.write_string(0, c as u16, *h).unwrap();
        }
        s.write_string(3, 1, "Sparse").unwrap(); // rows 2 and 3 left entirely blank
        s.write_string(3, 2, "Equity").unwrap();
        book.save(&path).unwrap();

        let data = read_assets_sheet(&path).unwrap();
        assert_eq!(data.rows.len(), 1);
        assert_eq!(data.rows[0].row_number, 4);
        assert_eq!(data.rows[0].label, "Sparse");
    }

    #[test]
    fn an_unrecognized_active_value_reads_as_active_but_falsey_tokens_do_not() {
        use rust_xlsxwriter::Workbook;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active_tokens.xlsx");
        let mut book = Workbook::new();
        let s = book.add_worksheet();
        s.set_name(SHEET_NAME).unwrap();
        for (c, h) in FIXED_HEADERS.iter().enumerate() {
            s.write_string(0, c as u16, *h).unwrap();
        }
        // Column 1 is label, column 5 is active.
        let cases = [
            ("Row No", "no"),
            ("Row N", "n"),
            ("Row False", "false"),
            ("Row Zero", "0"),
            ("Row Non", "non"), // French locale: this machine and its Excel are French
            ("Row Typo", "definitely not a real value"),
        ];
        for (i, (label, value)) in cases.iter().enumerate() {
            let row = (i + 1) as u32;
            s.write_string(row, 1, *label).unwrap();
            s.write_string(row, 5, *value).unwrap();
        }
        book.save(&path).unwrap();

        let data = read_assets_sheet(&path).unwrap();
        assert_eq!(data.rows.len(), 6);
        for r in &data.rows {
            if r.label == "Row Typo" {
                assert!(r.active, "an unrecognized value must lean active: silent data loss is worse than a visible typo");
            } else {
                assert!(!r.active, "row labeled '{}' should read as inactive", r.label);
            }
        }
    }
}
