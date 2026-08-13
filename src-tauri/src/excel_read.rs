use crate::error::{AppError, AppResult};
use crate::excel_gen::{bdh_sheet_name, sanitize_sheet_name, GenAsset, LAYOUT_VERSION};
use calamine::{open_workbook, Data, Reader, Xlsx};
use chrono::NaiveDate;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum CellValue { Num(f64), Text(String) }

#[derive(Debug, Clone)]
pub struct FieldSpec {
    pub field_id: i64,
    pub asset_class_id: i64,
    pub mnemonic: String,
    pub value_kind: String,
}

#[derive(Debug, Clone)]
pub struct ObsCell {
    pub asset_id: i64,
    pub field_id: i64,
    pub obs_date: NaiveDate,
    pub value: CellValue,
}

#[derive(Debug, Clone)]
pub struct CellProblem {
    pub asset_id: Option<i64>,
    pub field_id: Option<i64>,
    pub obs_date: Option<NaiveDate>,
    pub code: String,
    pub detail: String,
}

#[derive(Debug, Default)]
pub struct ReadOutcome {
    pub cells: Vec<ObsCell>,
    pub problems: Vec<CellProblem>,
}

#[derive(Debug)]
pub struct MetaRead {
    pub run_id: i64,
    pub view_id: i64,
    pub kind: String,
    pub layout_version: i64,
}

pub fn excel_serial_to_date(serial: f64) -> NaiveDate {
    NaiveDate::from_ymd_opt(1899, 12, 30).unwrap()
        + chrono::Duration::days(serial as i64)
}

pub fn classify_cell(data: &Data, value_kind: &str) -> Result<CellValue, (String, String)> {
    match data {
        Data::Empty => Err(("empty".into(), "empty cell".into())),
        Data::Error(e) => Err(("na".into(), format!("{e:?}"))),
        Data::String(s) => {
            let t = s.trim();
            if t.contains("Requesting Data") {
                Err(("requesting".into(), t.into()))
            } else if t.starts_with("#N/A Invalid Security") {
                Err(("invalid_security".into(), t.into()))
            } else if t.starts_with("#N/A Field Not Applicable") {
                Err(("field_not_applicable".into(), t.into()))
            } else if t.starts_with("#N/A") || t.starts_with("#NAME") || t.starts_with("#VALUE") {
                Err(("na".into(), t.into()))
            } else if t.is_empty() {
                Err(("empty".into(), "blank string".into()))
            } else {
                match value_kind {
                    "numeric" => t.replace(',', "").parse::<f64>()
                        .map(CellValue::Num)
                        .map_err(|_| ("type_mismatch".into(),
                                      format!("expected numeric, got '{t}'"))),
                    "date" => NaiveDate::parse_from_str(t, "%Y-%m-%d")
                        .or_else(|_| NaiveDate::parse_from_str(t, "%m/%d/%Y"))
                        .map(|d| CellValue::Text(d.format("%Y-%m-%d").to_string()))
                        .map_err(|_| ("type_mismatch".into(),
                                      format!("expected date, got '{t}'"))),
                    _ => Ok(CellValue::Text(t.into())),
                }
            }
        }
        Data::Float(f) => match value_kind {
            "numeric" => Ok(CellValue::Num(*f)),
            "date" => Ok(CellValue::Text(
                excel_serial_to_date(*f).format("%Y-%m-%d").to_string())),
            _ => Ok(CellValue::Text(f.to_string())),
        },
        Data::Int(i) => match value_kind {
            "numeric" => Ok(CellValue::Num(*i as f64)),
            "date" => Ok(CellValue::Text(
                excel_serial_to_date(*i as f64).format("%Y-%m-%d").to_string())),
            _ => Ok(CellValue::Text(i.to_string())),
        },
        Data::DateTime(dt) => {
            let d = excel_serial_to_date(dt.as_f64());
            match value_kind {
                "numeric" => Err(("type_mismatch".into(), "date where numeric expected".into())),
                _ => Ok(CellValue::Text(d.format("%Y-%m-%d").to_string())),
            }
        }
        other => Err(("na".into(), format!("unhandled cell {other:?}"))),
    }
}

pub fn read_meta(path: &Path) -> AppResult<MetaRead> {
    let mut wb: Xlsx<_> = open_workbook(path).map_err(|e: calamine::XlsxError| AppError::Excel(e.to_string()))?;
    let r = wb.worksheet_range("META")
        .map_err(|e| AppError::Validation(format!("META sheet missing: {e}")))?;
    let mut kv = HashMap::new();
    for row in r.rows() {
        if row.len() >= 2 {
            kv.insert(row[0].to_string(), row[1].to_string());
        }
    }
    let get_i64 = |k: &str| -> AppResult<i64> {
        kv.get(k).and_then(|v| v.parse().ok())
            .ok_or_else(|| AppError::Validation(format!("META missing {k}")))
    };
    Ok(MetaRead {
        run_id: get_i64("run_id")?,
        view_id: get_i64("view_id")?,
        kind: kv.get("kind").cloned().unwrap_or_default(),
        layout_version: get_i64("layout_version")?,
    })
}

fn check_meta(path: &Path, expected_run_id: i64) -> AppResult<()> {
    let m = read_meta(path)?;
    if m.run_id != expected_run_id {
        return Err(AppError::Validation(
            format!("META run_id {} != expected {expected_run_id}", m.run_id)));
    }
    if m.layout_version != LAYOUT_VERSION {
        return Err(AppError::Validation(
            format!("META layout_version {} != {LAYOUT_VERSION}", m.layout_version)));
    }
    Ok(())
}

pub fn read_eod_workbook(
    path: &Path, expected_run_id: i64, assets: &[GenAsset],
    fields: &[FieldSpec], obs_date: NaiveDate,
) -> AppResult<ReadOutcome> {
    check_meta(path, expected_run_id)?;
    let mut wb: Xlsx<_> = open_workbook(path).map_err(|e: calamine::XlsxError| AppError::Excel(e.to_string()))?;
    let by_security: HashMap<&str, &GenAsset> =
        assets.iter().map(|a| (a.bdp_security.as_str(), a)).collect();
    let mut out = ReadOutcome::default();

    let mut classes: Vec<(i64, String)> = assets.iter()
        .map(|a| (a.asset_class_id, a.class_name.clone())).collect();
    classes.sort();
    classes.dedup();

    for (class_id, class_name) in classes {
        let sheet = sanitize_sheet_name(&class_name);
        let r = wb.worksheet_range(&sheet)
            .map_err(|e| AppError::Validation(format!("sheet '{sheet}' missing: {e}")))?;
        // header row -> FieldSpec per column
        let header: Vec<String> = r.rows().next()
            .map(|row| row.iter().map(|c| c.to_string()).collect())
            .unwrap_or_default();
        let col_fields: Vec<Option<&FieldSpec>> = header.iter().skip(1)
            .map(|m| fields.iter()
                .find(|f| f.asset_class_id == class_id && f.mnemonic == *m))
            .collect();
        for row in r.rows().skip(1) {
            let sec = row.first().map(|c| c.to_string()).unwrap_or_default();
            let Some(asset) = by_security.get(sec.as_str()) else {
                out.problems.push(CellProblem {
                    asset_id: None, field_id: None, obs_date: Some(obs_date),
                    code: "unknown_security".into(),
                    detail: format!("row security '{sec}' not in view") });
                continue;
            };
            for (ci, fspec) in col_fields.iter().enumerate() {
                let Some(f) = fspec else { continue };
                let cell = row.get(ci + 1).unwrap_or(&Data::Empty);
                match classify_cell(cell, &f.value_kind) {
                    Ok(v) => out.cells.push(ObsCell {
                        asset_id: asset.asset_id, field_id: f.field_id,
                        obs_date, value: v }),
                    Err((code, detail)) => out.problems.push(CellProblem {
                        asset_id: Some(asset.asset_id), field_id: Some(f.field_id),
                        obs_date: Some(obs_date), code, detail }),
                }
            }
        }
    }
    Ok(out)
}

pub fn read_backfill_workbook(
    path: &Path, expected_run_id: i64, assets: &[GenAsset], fields: &[FieldSpec],
) -> AppResult<ReadOutcome> {
    check_meta(path, expected_run_id)?;
    let mut wb: Xlsx<_> = open_workbook(path).map_err(|e: calamine::XlsxError| AppError::Excel(e.to_string()))?;
    let mut out = ReadOutcome::default();

    for a in assets {
        let sheet = bdh_sheet_name(a.asset_id);
        let r = wb.worksheet_range(&sheet)
            .map_err(|e| AppError::Validation(format!("sheet '{sheet}' missing: {e}")))?;
        let rows: Vec<_> = r.rows().collect();
        let joined = rows.get(2).and_then(|row| row.get(1))
            .map(|c| c.to_string()).unwrap_or_default();
        let col_fields: Vec<Option<&FieldSpec>> = joined.split(',')
            .map(|m| fields.iter()
                .find(|f| f.asset_class_id == a.asset_class_id && f.mnemonic == m.trim()))
            .collect();
        for row in rows.iter().skip(4) {
            let date_cell = row.first().unwrap_or(&Data::Empty);
            if matches!(date_cell, Data::Empty) { continue; }  // past end of spill
            let obs_date = match classify_cell(date_cell, "date") {
                Ok(CellValue::Text(d)) =>
                    NaiveDate::parse_from_str(&d, "%Y-%m-%d").unwrap(),
                _ => {
                    out.problems.push(CellProblem {
                        asset_id: Some(a.asset_id), field_id: None, obs_date: None,
                        code: "bad_date".into(),
                        detail: format!("unparseable BDH date cell {date_cell:?}") });
                    continue;
                }
            };
            for (ci, fspec) in col_fields.iter().enumerate() {
                let Some(f) = fspec else { continue };
                let cell = row.get(ci + 1).unwrap_or(&Data::Empty);
                match classify_cell(cell, &f.value_kind) {
                    Ok(v) => out.cells.push(ObsCell {
                        asset_id: a.asset_id, field_id: f.field_id,
                        obs_date, value: v }),
                    Err((code, detail)) => out.problems.push(CellProblem {
                        asset_id: Some(a.asset_id), field_id: Some(f.field_id),
                        obs_date: Some(obs_date), code, detail }),
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::excel_gen::GenAsset;
    use calamine::Data;
    use chrono::NaiveDate;
    use rust_xlsxwriter::Workbook;

    #[test]
    fn classify_numeric_and_errors() {
        assert_eq!(classify_cell(&Data::Float(231.4), "numeric"), Ok(CellValue::Num(231.4)));
        assert_eq!(classify_cell(&Data::Int(5), "numeric"), Ok(CellValue::Num(5.0)));
        assert_eq!(classify_cell(&Data::String("hello".into()), "text"),
                   Ok(CellValue::Text("hello".into())));
        assert_eq!(classify_cell(&Data::Empty, "numeric").unwrap_err().0, "empty");
        assert_eq!(classify_cell(&Data::String("#N/A Invalid Security".into()), "numeric")
                   .unwrap_err().0, "invalid_security");
        assert_eq!(classify_cell(&Data::String("#N/A Field Not Applicable".into()), "numeric")
                   .unwrap_err().0, "field_not_applicable");
        assert_eq!(classify_cell(&Data::String("#N/A Requesting Data...".into()), "numeric")
                   .unwrap_err().0, "requesting");
        assert_eq!(classify_cell(&Data::String("not a number".into()), "numeric")
                   .unwrap_err().0, "type_mismatch");
    }

    #[test]
    fn classify_date_kinds() {
        // Excel serial for 2026-07-01
        let d = classify_cell(&Data::Float(46204.0), "date").unwrap();
        assert_eq!(d, CellValue::Text("2026-07-01".into()));
        assert_eq!(excel_serial_to_date(46204.0),
                   NaiveDate::from_ymd_opt(2026, 7, 1).unwrap());
    }

    #[test]
    fn classify_na_and_data_error() {
        // Bare #N/A string → "na" code
        assert_eq!(classify_cell(&Data::String("#N/A".into()), "numeric")
                   .unwrap_err().0, "na");
        // #NAME? string → "na" code
        assert_eq!(classify_cell(&Data::String("#NAME?".into()), "numeric")
                   .unwrap_err().0, "na");
        // #VALUE error → "na" code
        assert_eq!(classify_cell(&Data::String("#VALUE!".into()), "numeric")
                   .unwrap_err().0, "na");
        // calamine Data::Error variant → "na" code
        assert_eq!(classify_cell(&Data::Error(calamine::CellErrorType::NA), "numeric")
                   .unwrap_err().0, "na");
    }

    fn fixture_assets() -> (Vec<GenAsset>, Vec<FieldSpec>) {
        let assets = vec![GenAsset {
            asset_id: 1, asset_class_id: 10, class_name: "Equity".into(),
            label: "Apple".into(), bdp_security: "AAPL US Equity".into() }];
        let fields = vec![
            FieldSpec { field_id: 100, asset_class_id: 10,
                        mnemonic: "PX_LAST".into(), value_kind: "numeric".into() },
            FieldSpec { field_id: 101, asset_class_id: 10,
                        mnemonic: "PX_VOLUME".into(), value_kind: "numeric".into() },
        ];
        (assets, fields)
    }

    fn write_meta_fixture(wb: &mut Workbook, run_id: i64) {
        write_meta_fixture_with_layout(wb, run_id, "1");
    }

    fn write_meta_fixture_with_layout(wb: &mut Workbook, run_id: i64, layout_version: &str) {
        let s = wb.add_worksheet().set_name("META").unwrap();
        for (i, (k, v)) in [("run_id", run_id.to_string()), ("view_id", "3".into()),
                            ("kind", "eod".into()), ("generated_at", "t".into()),
                            ("layout_version", layout_version.into())].iter().enumerate() {
            s.write_string(i as u32, 0, *k).unwrap();
            s.write_string(i as u32, 1, v).unwrap();
        }
    }

    #[test]
    fn eod_read_mixes_values_and_problems() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refreshed.xlsx");
        let mut wb = Workbook::new();
        let s = wb.add_worksheet().set_name("Equity").unwrap();
        s.write_string(0, 0, "SECURITY").unwrap();
        s.write_string(0, 1, "PX_LAST").unwrap();
        s.write_string(0, 2, "PX_VOLUME").unwrap();
        s.write_string(1, 0, "AAPL US Equity").unwrap();
        s.write_number(1, 1, 231.4).unwrap();
        s.write_string(1, 2, "#N/A Invalid Security").unwrap();
        write_meta_fixture(&mut wb, 7);
        wb.save(&path).unwrap();

        let (assets, fields) = fixture_assets();
        let d = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let out = read_eod_workbook(&path, 7, &assets, &fields, d).unwrap();
        assert_eq!(out.cells.len(), 1);
        assert_eq!(out.cells[0].asset_id, 1);
        assert_eq!(out.cells[0].field_id, 100);
        assert_eq!(out.cells[0].obs_date, d);
        assert_eq!(out.cells[0].value, CellValue::Num(231.4));
        assert_eq!(out.problems.len(), 1);
        assert_eq!(out.problems[0].code, "invalid_security");
        assert_eq!(out.problems[0].field_id, Some(101));
    }

    #[test]
    fn eod_read_rejects_wrong_run_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.xlsx");
        let mut wb = Workbook::new();
        let s = wb.add_worksheet().set_name("Equity").unwrap();
        s.write_string(0, 0, "SECURITY").unwrap();
        write_meta_fixture(&mut wb, 999);
        wb.save(&path).unwrap();
        let (assets, fields) = fixture_assets();
        let d = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        assert!(read_eod_workbook(&path, 7, &assets, &fields, d).is_err());
    }

    #[test]
    fn eod_read_rejects_wrong_layout_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad_layout.xlsx");
        let mut wb = Workbook::new();
        let s = wb.add_worksheet().set_name("Equity").unwrap();
        s.write_string(0, 0, "SECURITY").unwrap();
        write_meta_fixture_with_layout(&mut wb, 7, "99");
        wb.save(&path).unwrap();
        let (assets, fields) = fixture_assets();
        let d = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let err = read_eod_workbook(&path, 7, &assets, &fields, d);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("layout_version"));
    }

    #[test]
    fn backfill_read_walks_bdh_spill() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bf.xlsx");
        let mut wb = Workbook::new();
        let s = wb.add_worksheet().set_name("A1").unwrap();
        s.write_string(0, 0, "asset_id").unwrap();
        s.write_string(0, 1, "1").unwrap();
        s.write_string(1, 0, "security").unwrap();
        s.write_string(1, 1, "AAPL US Equity").unwrap();
        s.write_string(2, 0, "fields").unwrap();
        s.write_string(2, 1, "PX_LAST,PX_VOLUME").unwrap();
        // simulated BDH spill from row 5: date serial | px_last | px_volume
        s.write_number(4, 0, 46204.0).unwrap();  // 2026-07-01
        s.write_number(4, 1, 230.0).unwrap();
        s.write_number(4, 2, 1000.0).unwrap();
        s.write_number(5, 0, 46205.0).unwrap();  // 2026-07-02
        s.write_number(5, 1, 231.0).unwrap();
        s.write_number(5, 2, 1100.0).unwrap();
        write_meta_fixture(&mut wb, 8);
        wb.save(&path).unwrap();

        let (assets, fields) = fixture_assets();
        let out = read_backfill_workbook(&path, 8, &assets, &fields).unwrap();
        assert_eq!(out.cells.len(), 4);  // 2 days x 2 fields
        assert!(out.problems.is_empty());
        let d1 = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        assert!(out.cells.iter().any(|c|
            c.obs_date == d1 && c.field_id == 100 && c.value == CellValue::Num(230.0)));
    }

    #[test]
    fn backfill_read_handles_error_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bf_errors.xlsx");
        let mut wb = Workbook::new();
        let s = wb.add_worksheet().set_name("A1").unwrap();
        s.write_string(0, 0, "asset_id").unwrap();
        s.write_string(0, 1, "1").unwrap();
        s.write_string(1, 0, "security").unwrap();
        s.write_string(1, 1, "AAPL US Equity").unwrap();
        s.write_string(2, 0, "fields").unwrap();
        s.write_string(2, 1, "PX_LAST,PX_VOLUME").unwrap();
        // Row 4: bad date in column A (not a number)
        s.write_string(4, 0, "not a date").unwrap();
        s.write_number(4, 1, 230.0).unwrap();
        s.write_number(4, 2, 1000.0).unwrap();
        // Row 5: valid date, but PX_VOLUME has "#N/A Requesting Data..." error
        s.write_number(5, 0, 46205.0).unwrap();  // 2026-07-02
        s.write_number(5, 1, 231.0).unwrap();
        s.write_string(5, 2, "#N/A Requesting Data...").unwrap();
        write_meta_fixture(&mut wb, 8);
        wb.save(&path).unwrap();

        let (assets, fields) = fixture_assets();
        let out = read_backfill_workbook(&path, 8, &assets, &fields).unwrap();
        // Row 4: bad date → 1 problem ("bad_date")
        let bad_date_problems: Vec<_> = out.problems.iter()
            .filter(|p| p.code == "bad_date").collect();
        assert_eq!(bad_date_problems.len(), 1);
        // Row 5: valid date, 1 valid obs (PX_LAST), 1 problem (PX_VOLUME "requesting")
        let requesting_problems: Vec<_> = out.problems.iter()
            .filter(|p| p.code == "requesting").collect();
        assert_eq!(requesting_problems.len(), 1);
        assert_eq!(requesting_problems[0].field_id, Some(101));
        // 1 valid observation from row 5
        let d2 = NaiveDate::from_ymd_opt(2026, 7, 2).unwrap();
        assert!(out.cells.iter().any(|c|
            c.obs_date == d2 && c.field_id == 100 && c.value == CellValue::Num(231.0)));
    }
}
