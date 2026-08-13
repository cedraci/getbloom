use crate::error::{AppError, AppResult};
use rust_xlsxwriter::{Formula, Workbook};
use std::collections::BTreeMap;
use std::path::Path;

pub const LAYOUT_VERSION: i64 = 1;

#[derive(Debug, Clone)]
pub struct WbMeta {
    pub run_id: i64,
    pub view_id: i64,
    pub kind: String,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct GenAsset {
    pub asset_id: i64,
    pub asset_class_id: i64,
    pub class_name: String,
    pub label: String,
    pub bdp_security: String,
}

#[derive(Debug, Clone)]
pub struct GenField {
    pub field_id: i64,
    pub asset_class_id: i64,
    pub mnemonic: String,
}

pub fn sanitize_sheet_name(raw: &str) -> String {
    let cleaned: String = raw.chars()
        .filter(|c| !matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\'))
        .take(31)
        .collect();
    if cleaned.trim().is_empty() { "Sheet".to_string() } else { cleaned }
}

fn write_meta(wb: &mut Workbook, meta: &WbMeta) -> AppResult<()> {
    let s = wb.add_worksheet().set_name("META").map_err(|e| AppError::Excel(e.to_string()))?;
    let rows: [(&str, String); 5] = [
        ("run_id", meta.run_id.to_string()),
        ("view_id", meta.view_id.to_string()),
        ("kind", meta.kind.clone()),
        ("generated_at", meta.generated_at.clone()),
        ("layout_version", LAYOUT_VERSION.to_string()),
    ];
    for (i, (k, v)) in rows.iter().enumerate() {
        s.write_string(i as u32, 0, *k).map_err(|e| AppError::Excel(e.to_string()))?;
        s.write_string(i as u32, 1, v).map_err(|e| AppError::Excel(e.to_string()))?;
    }
    s.set_hidden(true);
    Ok(())
}

pub fn generate_eod_workbook(
    path: &Path, meta: &WbMeta, assets: &[GenAsset], fields: &[GenField],
) -> AppResult<()> {
    if assets.is_empty() {
        return Err(AppError::Validation("view has no active assets".into()));
    }
    let mut wb = Workbook::new();

    // group by class, preserving stable order via BTreeMap on class id
    let mut by_class: BTreeMap<i64, (String, Vec<&GenAsset>)> = BTreeMap::new();
    for a in assets {
        by_class.entry(a.asset_class_id)
            .or_insert_with(|| (a.class_name.clone(), Vec::new()))
            .1.push(a);
    }

    for (class_id, (class_name, class_assets)) in &by_class {
        let class_fields: Vec<&GenField> =
            fields.iter().filter(|f| f.asset_class_id == *class_id).collect();
        if class_fields.is_empty() {
            return Err(AppError::Validation(
                format!("no fields configured for asset class '{class_name}'")));
        }
        let sheet = wb.add_worksheet()
            .set_name(sanitize_sheet_name(class_name))
            .map_err(|e| AppError::Excel(e.to_string()))?;
        sheet.write_string(0, 0, "SECURITY").map_err(|e| AppError::Excel(e.to_string()))?;
        for (ci, f) in class_fields.iter().enumerate() {
            sheet.write_string(0, (ci + 1) as u16, &f.mnemonic)
                .map_err(|e| AppError::Excel(e.to_string()))?;
        }
        for (ri, a) in class_assets.iter().enumerate() {
            let row = (ri + 1) as u32;
            sheet.write_string(row, 0, &a.bdp_security)
                .map_err(|e| AppError::Excel(e.to_string()))?;
            for (ci, f) in class_fields.iter().enumerate() {
                let formula = format!("=BDP($A{},\"{}\")", row + 1, f.mnemonic);
                sheet.write_formula(row, (ci + 1) as u16, Formula::new(formula))
                    .map_err(|e| AppError::Excel(e.to_string()))?;
            }
        }
    }

    write_meta(&mut wb, meta)?;
    wb.save(path).map_err(|e| AppError::Excel(e.to_string()))?;
    Ok(())
}

pub fn bdh_sheet_name(asset_id: i64) -> String {
    format!("A{asset_id}")
}

pub fn generate_backfill_workbook(
    path: &Path, meta: &WbMeta, assets: &[GenAsset], fields: &[GenField],
    start: chrono::NaiveDate, end: chrono::NaiveDate,
) -> AppResult<()> {
    if assets.is_empty() {
        return Err(AppError::Validation("view has no active assets".into()));
    }
    if start > end {
        return Err(AppError::Validation("backfill start date after end date".into()));
    }
    let (s, e) = (start.format("%Y%m%d").to_string(), end.format("%Y%m%d").to_string());
    let mut wb = Workbook::new();

    for a in assets {
        let mnemonics: Vec<&str> = fields.iter()
            .filter(|f| f.asset_class_id == a.asset_class_id)
            .map(|f| f.mnemonic.as_str())
            .collect();
        if mnemonics.is_empty() {
            return Err(AppError::Validation(
                format!("no fields configured for asset '{}'", a.label)));
        }
        let joined = mnemonics.join(",");
        let sheet = wb.add_worksheet()
            .set_name(bdh_sheet_name(a.asset_id))
            .map_err(|er| AppError::Excel(er.to_string()))?;
        sheet.write_string(0, 0, "asset_id").map_err(|er| AppError::Excel(er.to_string()))?;
        sheet.write_string(0, 1, a.asset_id.to_string()).map_err(|er| AppError::Excel(er.to_string()))?;
        sheet.write_string(1, 0, "security").map_err(|er| AppError::Excel(er.to_string()))?;
        sheet.write_string(1, 1, &a.bdp_security).map_err(|er| AppError::Excel(er.to_string()))?;
        sheet.write_string(2, 0, "fields").map_err(|er| AppError::Excel(er.to_string()))?;
        sheet.write_string(2, 1, &joined).map_err(|er| AppError::Excel(er.to_string()))?;
        let formula = format!(
            "=BDH(\"{}\",\"{}\",\"{}\",\"{}\",\"Dates=S\")",
            a.bdp_security, joined, s, e);
        sheet.write_formula(4, 0, Formula::new(formula))
            .map_err(|er| AppError::Excel(er.to_string()))?;
    }

    write_meta(&mut wb, meta)?;
    wb.save(path).map_err(|er| AppError::Excel(er.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use calamine::{open_workbook, Reader, Xlsx};

    fn sample() -> (Vec<GenAsset>, Vec<GenField>) {
        let assets = vec![
            GenAsset { asset_id: 1, asset_class_id: 10, class_name: "Equity".into(),
                       label: "Apple".into(), bdp_security: "AAPL US Equity".into() },
            GenAsset { asset_id: 2, asset_class_id: 10, class_name: "Equity".into(),
                       label: "LVMH".into(), bdp_security: "/isin/FR0000121014 Equity".into() },
            GenAsset { asset_id: 3, asset_class_id: 20, class_name: "Index".into(),
                       label: "EuroStoxx".into(), bdp_security: "SX5E Index".into() },
        ];
        let fields = vec![
            GenField { field_id: 100, asset_class_id: 10, mnemonic: "PX_LAST".into() },
            GenField { field_id: 101, asset_class_id: 10, mnemonic: "PX_VOLUME".into() },
            GenField { field_id: 200, asset_class_id: 20, mnemonic: "PX_LAST".into() },
        ];
        (assets, fields)
    }

    #[test]
    fn sheet_name_sanitized() {
        assert_eq!(sanitize_sheet_name("FX/Rates: EUR*"), "FXRates EUR");
        assert_eq!(sanitize_sheet_name(""), "Sheet");
        assert_eq!(sanitize_sheet_name(&"x".repeat(40)).len(), 31);
    }

    #[test]
    fn eod_workbook_has_one_sheet_per_class_plus_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wb.xlsx");
        let (assets, fields) = sample();
        let meta = WbMeta { run_id: 7, view_id: 3, kind: "eod".into(),
                            generated_at: "2026-08-13T10:00:00".into() };
        generate_eod_workbook(&path, &meta, &assets, &fields).unwrap();

        let mut wb: Xlsx<_> = open_workbook(&path).unwrap();
        let names = wb.sheet_names().to_vec();
        assert!(names.contains(&"Equity".to_string()));
        assert!(names.contains(&"Index".to_string()));
        assert!(names.contains(&"META".to_string()));

        // header row + securities in column A
        let r = wb.worksheet_range("Equity").unwrap();
        assert_eq!(r.get_value((0, 0)).unwrap().to_string(), "SECURITY");
        assert_eq!(r.get_value((0, 1)).unwrap().to_string(), "PX_LAST");
        assert_eq!(r.get_value((1, 0)).unwrap().to_string(), "AAPL US Equity");

        // BDP formulas present
        let f = wb.worksheet_formula("Equity").unwrap();
        let cell = f.get_value((1, 1)).unwrap().to_string();
        assert!(cell.contains("BDP($A2,\"PX_LAST\")"), "got formula: {cell}");

        // META carries run identity
        let m = wb.worksheet_range("META").unwrap();
        assert_eq!(m.get_value((0, 0)).unwrap().to_string(), "run_id");
        assert_eq!(m.get_value((0, 1)).unwrap().to_string(), "7");
        assert_eq!(m.get_value((4, 0)).unwrap().to_string(), "layout_version");
        assert_eq!(m.get_value((4, 1)).unwrap().to_string(), "1");
    }

    #[test]
    fn backfill_workbook_one_sheet_per_asset_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bf.xlsx");
        let (assets, fields) = sample();
        let meta = WbMeta { run_id: 8, view_id: 3, kind: "backfill".into(),
                            generated_at: "2026-08-13T10:00:00".into() };
        let start = chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let end = chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap();
        generate_backfill_workbook(&path, &meta, &assets, &fields, start, end).unwrap();

        let mut wb: Xlsx<_> = open_workbook(&path).unwrap();
        let names = wb.sheet_names().to_vec();
        // one sheet per asset + META — all inside the single workbook
        assert!(names.contains(&"A1".to_string()));
        assert!(names.contains(&"A2".to_string()));
        assert!(names.contains(&"A3".to_string()));
        assert!(names.contains(&"META".to_string()));

        let r = wb.worksheet_range("A1").unwrap();
        assert_eq!(r.get_value((1, 1)).unwrap().to_string(), "AAPL US Equity");
        assert_eq!(r.get_value((2, 1)).unwrap().to_string(), "PX_LAST,PX_VOLUME");

        let f = wb.worksheet_formula("A1").unwrap();
        let cell = f.get_value((4, 0)).unwrap().to_string();
        assert!(cell.contains("BDH(\"AAPL US Equity\",\"PX_LAST,PX_VOLUME\",\"20260701\",\"20260731\",\"Dates=S\")"),
                "got formula: {cell}");
    }

    #[test]
    fn backfill_rejects_reversed_range() {
        let dir = tempfile::tempdir().unwrap();
        let (assets, fields) = sample();
        let meta = WbMeta { run_id: 8, view_id: 3, kind: "backfill".into(),
                            generated_at: "t".into() };
        let start = chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap();
        let end = chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        assert!(generate_backfill_workbook(&dir.path().join("x.xlsx"),
                &meta, &assets, &fields, start, end).is_err());
    }
}
