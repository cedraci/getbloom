> **SUPERSEDED (2026-08-20).** This is the pre-P1 draft; it still references
> the deleted `asset` table, TimescaleDB, and the removed Assets screen.
> The live, executed record is
> `docs/superpowers/plans/2026-08-19-p1-smoke-checklist.md`.

# Bloomberg EOD pipeline — end-to-end smoke test

Rewritten for Amendment A2 (BLPAPI). The Excel/COM version of this checklist is
obsolete: there is no workbook, no add-in macro, no `#N/A` strings, and no
Excel process to orphan.

## Prerequisites

- **Bloomberg Terminal** running and logged in as the current Windows user
  (Desktop API listens on `localhost:8194`; `bbcomm.exe` must be running)
- **Python** with the `blpapi` package:
  `pip install --index-url=https://blpapi.bloomberg.com/repository/releases/python/simple/ blpapi`
- **PostgreSQL** with the `bloomdata` database created
- **`BLOOM_DATABASE_URL`** set (defaults to
  `postgres://postgres:postgres@localhost/bloomdata`)
- TimescaleDB is **optional** — `0001_init.sql` uses it when present and
  continues without it otherwise (it ships no Windows build).

**Amendment A1 semantics:** a daily run targets the **previous trading day**,
never today's live values. A Monday run reports Friday's close. The schedule
window therefore has no relationship to market close and can sit anywhere in
the day.

## Automated first

Most of what this checklist used to verify by hand is now covered by tests.
Run these before touching the UI:

```bash
cd src-tauri

# 1. Sidecar parsing — no Terminal, no blpapi module needed
python -m unittest discover -s scripts -p "test_*.py"

# 2. Rust unit + database integration tests
export BLOOM_TEST_DATABASE_URL='postgres://postgres:postgres@localhost/bloom_test'
cargo test -- --include-ignored

# 3. The live end-to-end test: real Terminal -> real BLPAPI -> real database
cargo test --test db_integration smoke_real_bloomberg -- --ignored --nocapture
```

Step 3 is the whole pipeline in one command. It is idempotent — fixtures are
looked up before being created — so it can be re-run freely.

## Manual checklist

- [ ] **Step 0: Probe** — `python scripts/blp_fetch.py --probe`.
      Expect `GREEN LIGHT` in about a second. If this fails, nothing below
      will work; the message names which stage broke (module, session,
      service, or entitlement).

- [ ] **Step 1: Seed** — in the UI: class `Equity`; fields `PX_LAST`
      (numeric), `PX_VOLUME` (numeric), `NAME` (text); assets `AAPL US`
      (ticker) and one ISIN-based asset (e.g. `FR0000120271`, yellow key
      `Equity`); view `smoke` containing both.

- [ ] **Step 2: Estimate** — Run tab shows ~6 estimated hits (2 assets ×
      3 fields), level Ok.

- [ ] **Step 3: Run** — press **Run now**. It should complete in seconds, not
      minutes. No Excel involved, nothing to watch in Task Manager.

- [ ] **Step 4: Verify DB** —
      `psql bloomdata -c "SELECT a.label, f.mnemonic, o.obs_date, o.value_num, o.value_text FROM observation o JOIN asset a ON a.id=o.asset_id JOIN field_def f ON f.id=o.field_id ORDER BY a.label, f.mnemonic;"`
      Expect 6 rows with plausible values and run status `ok`.
      **`obs_date` must be the previous trading day**, and `PX_LAST` (history)
      and `NAME` (reference) must carry the *same* date — otherwise one
      asset-day would split across two primary keys.

- [ ] **Step 5: Audit trail** — `run.payload_path` points at
      `archive/<YYYY>/<MM>/run_<id>_smoke_<date>.json`, and that file contains
      the raw Bloomberg response. Copying it into
      `src-tauri/tests/fixtures/blpapi/` turns it into a regression test
      unchanged — that is the format `--replay` reads.

- [ ] **Step 6: Idempotency in anger** — press **Run now** again. Row count in
      `observation` is unchanged (still 6); `hit_ledger` shows two entries
      totalling ~12.

- [ ] **Step 7: Backfill** — pick a range from the gap panel → **Backfill** →
      confirm the shown cost. Verify `observation` gains trading-day rows only
      (holidays absent — Bloomberg's calendar wins). **Text fields are
      deliberately absent from a backfill**: one live `NAME` value stamped
      across 30 days would fabricate history that was never observed. Daily
      runs fill them in.

- [ ] **Step 8: Schedule** — Settings: schedule the `smoke` view with window
      `09:00–18:00`. Check the `schedule` row has `drawn_for = today` and a
      `drawn_at` inside the window; restart the app; `drawn_at` unchanged
      (no re-roll).

## Failure modes worth provoking once

These are the ones that bit us during development; each has an automated test,
but seeing them by hand is cheap.

- [ ] **Terminal closed** — stop the Terminal and run. Expect exit `3`, run
      status `failed`, and `error_summary` naming the session failure. It must
      **not** report success with zero rows.

- [ ] **Bad ticker** — add an asset `ZQZQZQ99 US` and run. Expect an
      `ingest_issue` with code `invalid_security`, run status `partial`, and
      the good assets still ingested.

- [ ] **Holiday** — run for a date the market was closed. Expect `no_data`
      issues and `partial`. Note that `no_data` is **not** proof of a holiday:
      a security that resolves but has no data that day looks identical.
      Nothing should ever infer a market calendar from it.

## Known gaps (not covered by any of the above)

- The scheduler only ticks while the app is running; there is no autostart,
  tray icon, or service.
- A failed or persistently `partial` run surfaces only as a `last_result`
  string in Settings; there is no alerting.
- Missed days are not automatically caught up; only a manual Backfill
  recovers them.
