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
}
