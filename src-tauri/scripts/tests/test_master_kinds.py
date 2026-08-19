"""Sidecar parsing for the security-master request kinds.

Replay only: no Bloomberg session is opened. The fixtures are the P0 captures,
so a change in parsing that would break against the real Terminal breaks here.

pytest is not installed on this machine, so this file also carries a
__main__ block that runs every test_* function directly and reports
pass/fail, mirroring the pattern in ../test_blp_fetch.py:

    cd src-tauri/scripts && python tests/test_master_kinds.py -v
"""
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "src-tauri" / "scripts"))
FACTS = ROOT / "docs" / "superpowers" / "specs" / "blpapi-facts"

import blp_fetch  # noqa: E402


def load(name):
    with open(FACTS / name, encoding="utf-8") as fh:
        return json.load(fh)


def test_bulk_field_rows_are_parsed_as_tables_not_scalars():
    """HISTORICAL_IDS_TIME_RANGE is ftype BulkFormat: its value is a list of
    dicts, not a number. Parsing it as a scalar would silently lose the whole
    identifier history."""
    cap = load("histids_report.json")
    msgs = cap["META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT', "
               "'HISTORICAL_STARTING_IDENTIFIER']"]
    rows, problems = [], []
    for m in msgs:
        blp_fetch.parse_bulk_message(
            {"kind": "bulk_reference", "fields": ["HISTORICAL_IDS_TIME_RANGE"]},
            m, rows, problems)
    assert problems == []
    assert len(rows) == 1
    entry = rows[0]
    assert entry["security"] == "META US Equity"
    assert entry["field"] == "HISTORICAL_IDS_TIME_RANGE"
    assert entry["rows"] == [{
        "Date": "2022-06-09", "Old ID": "FB", "New ID": "META",
        "Old Exch": "US", "New Exch": "US",
        "Action ID": "228233742", "Source": "ID Change",
    }]


def test_the_anchored_and_unanchored_answers_differ():
    """P0 6.4. The same query about META US Equity returns Facebook's rename
    when anchored and the Roundhill ETF's rename when not. The sidecar must
    return both faithfully -- deciding which is trustworthy is Rust's job."""
    cap = load("histids_report.json")
    anchored, unanchored = [], []
    for key, sink in ((
        "META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT', "
        "'HISTORICAL_STARTING_IDENTIFIER']", anchored),
            ("META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT']", unanchored)):
        for m in cap[key]:
            blp_fetch.parse_bulk_message(
                {"kind": "bulk_reference", "fields": ["HISTORICAL_IDS_TIME_RANGE"]},
                m, sink, [])
    assert anchored[0]["rows"][0]["New ID"] == "META"
    assert unanchored[0]["rows"][0]["New ID"] == "METV", (
        "the unanchored answer is a different company entirely")


def test_a_missing_bulk_field_is_a_problem_not_an_empty_table():
    msg = {"securityData": [{"security": "XYZ US Equity", "fieldData": {},
                             "fieldExceptions": [], "sequenceNumber": 0}]}
    rows, problems = [], []
    blp_fetch.parse_bulk_message(
        {"kind": "bulk_reference", "fields": ["HISTORICAL_IDS_TIME_RANGE"]},
        msg, rows, problems)
    assert rows == []
    assert len(problems) == 1
    assert problems[0]["code"] == "no_data"


def test_a_security_error_on_a_bulk_request_is_attributed_to_that_security():
    msg = {"securityData": [{
        "security": "NOPE US Equity",
        "securityError": {"category": "BAD_SEC", "subcategory": "INVALID_SECURITY",
                          "message": "Unknown/Invalid Security"},
        "fieldData": {}, "fieldExceptions": [], "sequenceNumber": 0}]}
    rows, problems = [], []
    blp_fetch.parse_bulk_message(
        {"kind": "bulk_reference", "fields": ["HISTORICAL_IDS_TIME_RANGE"]},
        msg, rows, problems)
    assert rows == []
    assert problems[0]["code"] == "invalid_security"
    assert problems[0]["security"] == "NOPE US Equity"


def test_instrument_list_results_are_parsed():
    msg = {"results": [
        {"security": "AAPL US<equity>", "description": "Apple Inc"},
        {"security": "AAPL LN<equity>", "description": "Apple Inc"},
    ]}
    out = []
    blp_fetch.parse_instrument_list_message(msg, out)
    assert out == [
        {"security": "AAPL US<equity>", "description": "Apple Inc"},
        {"security": "AAPL LN<equity>", "description": "Apple Inc"},
    ], "the raw form is preserved; Rust normalises it (never the reverse)"


def test_validation_rejects_an_instrument_list_without_a_query():
    errs = blp_fetch.validate_request_spec({"kind": "instrument_list"})
    assert errs and "query" in errs[0]


def test_validation_accepts_the_new_kinds():
    assert blp_fetch.validate_request_spec({
        "kind": "instrument_list", "query": "AAPL", "max_results": 10}) == []
    assert blp_fetch.validate_request_spec({
        "kind": "bulk_reference", "securities": ["AAPL US Equity"],
        "fields": ["EQY_DVD_ADJUST_FACT"]}) == []


if __name__ == "__main__":
    verbose = "-v" in sys.argv
    tests = [(name, obj) for name, obj in sorted(globals().items())
              if name.startswith("test_") and callable(obj)]
    failures = []
    for name, fn in tests:
        try:
            fn()
        except AssertionError as e:
            failures.append((name, e))
            print(f"FAIL: {name}: {e}")
        except Exception as e:  # noqa: BLE001 - report, don't hide, unexpected errors
            failures.append((name, e))
            print(f"ERROR: {name}: {type(e).__name__}: {e}")
        else:
            if verbose:
                print(f"ok: {name}")
    print(f"\n{len(tests) - len(failures)}/{len(tests)} passed")
    sys.exit(1 if failures else 0)
