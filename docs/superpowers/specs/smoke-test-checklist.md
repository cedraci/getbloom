# Bloomberg EOD pipeline — end-to-end smoke test

## Prerequisites

Before running this checklist, ensure:

- **Bloomberg Terminal** is logged in on the machine
- **Excel** with the Bloomberg add-in is installed
- **PostgreSQL 16** is installed
- **TimescaleDB** is installed (Windows installer from timescale.com)
- **`bloomdata` database** is created (`createdb bloomdata`)
- **`BLOOM_DATABASE_URL`** environment variable is set (or defaults to PostgreSQL connection string)

**Note**: an EOD run scheduled inside 09:00–18:00 snapshots LIVE BDP values during market hours and stores them under today's date — schedule the window after your market's close (or accept snapshot semantics) if downstream statistics assume closing prices.

## Smoke Test Steps

- [ ] **Step 1: Seed** — in the UI: class `Equity`; fields `PX_LAST` (numeric), `PX_VOLUME` (numeric), `NAME` (text); assets `AAPL US` (ticker) and one ISIN-based asset (e.g. `FR0000120271` / yellow key `Equity`); view `smoke` with both assets.

- [ ] **Step 2: Estimate** — Run tab shows ~6 estimated hits (2 assets × 3 fields), level Ok.

- [ ] **Step 3: Run** — press **Run now**. Watch: `pending/` gains the single workbook (ONE file for both assets), Excel flashes in Task Manager and exits, no orphan `EXCEL.EXE` remains.

- [ ] **Step 4: Verify DB** —
  `psql bloomdata -c "SELECT a.label, f.mnemonic, o.obs_date, o.value_num, o.value_text FROM observation o JOIN asset a ON a.id=o.asset_id JOIN field_def f ON f.id=o.field_id ORDER BY a.label, f.mnemonic;"`
  Expected: 6 rows, today's date, plausible values; run status `ok` in the history table; workbook moved into `archive/<YYYY>/<MM>/`.

- [ ] **Step 5: Idempotency in anger** — press **Run now** again; row count in `observation` unchanged (still 6), `hit_ledger` shows two entries totaling ~12.

- [ ] **Step 6: Backfill** — delete yesterday's rows is not needed; instead pick the gap panel (fresh DB shows a gap range) → **Backfill** → confirm the shown cost → verify `observation` gains weekday rows only (holidays absent — Bloomberg's calendar wins).

- [ ] **Step 7: Schedule** — Settings: schedule the `smoke` view with window `09:00–18:00`; check the `schedule` row has `drawn_for = today` and a `drawn_at` inside the window; restart the app; `drawn_at` unchanged (no re-roll).

## Verify against installed add-in

**Macro name verification**: The Bloomberg add-in's refresh macro name is `RefreshAllStaticData`. Confirm this matches the installed add-in version by checking:
- Location: `src-tauri/scripts/refresh.ps1` — exactly one place by design

**Error string verification**: The exact `#N/A` strings used for error handling in the pipeline must match the installed Bloomberg add-in version. Confirm by checking:
- Location: `excel_read::classify_cell` in the Rust code — exactly one place by design

**Task 7 verification items**:
- Verify the exit-2 timeout path is correctly implemented in the shutdown sequence
- Confirm that `$excel.Hwnd` is populated with `Visible=$false` on the real machine during Excel automation
- Verify the refresh poll loop in `refresh.ps1` actually WAITS (does not exit on the first poll) while cells still show "Requesting Data" — a fresh COM instance without explicit `Find()` LookIn/LookAt args can search formulas instead of values and exit the loop instantly
