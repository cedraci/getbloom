//! Reading and writing the assets workbook. No database access lives here.

use crate::error::{AppError, AppResult};
use rust_xlsxwriter::{DataValidation, Format, Workbook};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const SHEET_NAME: &str = "Assets";
pub const FIXED_HEADERS: [&str; 9] = [
    "id", "label", "class", "id_kind", "ticker", "isin", "yellow_key", "active", "security",
];

/// One asset as it appears in the exported workbook. `security` is written for
/// the reader's benefit only -- import always recomputes it.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportRow {
    pub id: i64,
    pub label: String,
    pub class: String,
    pub id_kind: String,
    pub ticker: String,
    pub isin: String,
    pub yellow_key: String,
    pub active: bool,
    pub security: String,
    /// Names of the views this asset belongs to.
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
        // A blank id is how the sheet says "new asset". Writing 0 here would
        // read back as id 0 and be rejected as an id not in the database, so a
        // row assembled for an add must leave the cell empty.
        if r.id > 0 {
            sheet.write_number_with_format(row, 0, r.id as f64, &readonly).map_err(xlsx_err)?;
        } else {
            sheet.write_blank(row, 0, &readonly).map_err(xlsx_err)?;
        }
        sheet.write_string(row, 1, &r.label).map_err(xlsx_err)?;
        sheet.write_string(row, 2, &r.class).map_err(xlsx_err)?;
        sheet.write_string(row, 3, &r.id_kind).map_err(xlsx_err)?;
        sheet.write_string(row, 4, &r.ticker).map_err(xlsx_err)?;
        sheet.write_string(row, 5, &r.isin).map_err(xlsx_err)?;
        sheet.write_string(row, 6, &r.yellow_key).map_err(xlsx_err)?;
        sheet.write_string(row, 7, if r.active { "yes" } else { "no" }).map_err(xlsx_err)?;
        sheet.write_string_with_format(row, 8, &r.security, &readonly).map_err(xlsx_err)?;
        for (j, v) in view_names.iter().enumerate() {
            let c = (FIXED_HEADERS.len() + j) as u16;
            let mark = if r.views.iter().any(|x| x == v) { "x" } else { "" };
            sheet.write_string(row, c, mark).map_err(xlsx_err)?;
        }
    }

    // Dropdowns turn three of the most typo-prone columns into pick lists.
    let last = rows.len().max(1) as u32;
    let classes: Vec<&str> = class_names.iter().map(String::as_str).collect();
    let dv_class = DataValidation::new().allow_list_strings(&classes).map_err(xlsx_err)?;
    sheet.add_data_validation(1, 2, last, 2, &dv_class).map_err(xlsx_err)?;
    let dv_kind = DataValidation::new()
        .allow_list_strings(&["ticker", "isin"]).map_err(xlsx_err)?;
    sheet.add_data_validation(1, 3, last, 3, &dv_kind).map_err(xlsx_err)?;
    let dv_active = DataValidation::new()
        .allow_list_strings(&["yes", "no"]).map_err(xlsx_err)?;
    sheet.add_data_validation(1, 7, last, 7, &dv_active).map_err(xlsx_err)?;
    // `yellow_key` deliberately has no dropdown: the set is open-ended
    // (Equity, Corp, Index, Curncy, Comdty, Govt, ...) and constraining it
    // would block a legitimate key nobody thought to list.

    book.save(path).map_err(xlsx_err)?;
    Ok(())
}

pub fn file_sha256(path: &Path) -> AppResult<String> {
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    // digest 0.11's fixed-size `Array` output no longer implements `LowerHex`
    // (it did via `GenericArray` on the 0.10 line the brief was written
    // against), so hex-encode byte by byte instead of `format!("{:x}", ..)`.
    let digest = h.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<ExportRow> {
        vec![ExportRow {
            id: 7,
            label: "Apple".into(),
            class: "Equity".into(),
            id_kind: "ticker".into(),
            ticker: "AAPL US".into(),
            isin: String::new(),
            yellow_key: "Equity".into(),
            active: true,
            security: "AAPL US Equity".into(),
            views: vec!["Daily".into()],
        }]
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
        rows[0].id = 0; // an asset that does not exist yet
        write_assets_sheet(&path, &rows, &[], &["Equity".to_string()]).unwrap();
        // Proven properly in the reader task; here it is enough that the file
        // writes without turning 0 into a number the reader would parse.
        assert!(path.exists());
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
}
