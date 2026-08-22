"""Tests for the BLPAPI sidecar's parsing layer.

Runs with no blpapi module and no Bloomberg Terminal -- the point of keeping
element conversion separate from parsing.

Every fixture prefixed `real_` is a genuine response captured from the
Bloomberg Desktop API on 2026-08-18 via `--raw-out`, unedited. That format is
exactly what `--replay` consumes, so a real capture becomes a regression test
by copying the file in. `response_error.json` is the one synthetic fixture: a
request-level responseError could not be provoked on demand (see
test_bad_date_is_rejected_before_it_can_be_mistaken_for_a_holiday for why).

    cd src-tauri && python -m unittest discover -s scripts -p "test_*.py" -v
"""

import json
import os
import sys
import unittest
from collections import Counter

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import blp_fetch  # noqa: E402

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                        "..", "tests", "fixtures", "blpapi")

# Live wire captures from the 2026-08-22 probe (spec F1/F6), committed
# alongside the sidecar itself rather than under tests/fixtures/blpapi --
# these are `--raw-out` captures used both as audit trail and as --replay
# input, not synthetic canned structures.
LIVE_FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                             "fixtures")


def load(name):
    with open(os.path.join(FIXTURES, name), encoding="utf-8") as fh:
        return json.load(fh)


def load_live(name):
    with open(os.path.join(LIVE_FIXTURES, name), encoding="utf-8") as fh:
        return json.load(fh)


def by_key(rows):
    return {(r["security"], r["field"], r["date"]): r for r in rows}


class DateTests(unittest.TestCase):
    def test_iso_date_accepts_both_forms(self):
        self.assertEqual(blp_fetch.iso_date("20260814"), "2026-08-14")
        self.assertEqual(blp_fetch.iso_date("2026-08-14"), "2026-08-14")
        self.assertEqual(blp_fetch.iso_date("2026-08-14T00:00:00"), "2026-08-14")

    def test_iso_date_rejects_impossible_dates(self):
        # Bloomberg accepts 20261301 silently and returns an empty result, so
        # a lenient parser would mint a `no_data` issue dated 2026-13-01.
        self.assertIsNone(blp_fetch.iso_date("20261301"))
        self.assertIsNone(blp_fetch.iso_date("20260230"))
        self.assertIsNone(blp_fetch.iso_date("garbage"))
        self.assertIsNone(blp_fetch.iso_date(None))

    def test_validation_catches_bad_requests(self):
        self.assertEqual(blp_fetch.validate_payload({}), ["payload has no 'requests'"])
        errs = blp_fetch.validate_payload({"requests": [
            {"kind": "history", "securities": ["A"], "fields": ["F"],
             "start": "20260814", "end": "20260801"}]})
        self.assertTrue(any("after end" in e for e in errs), errs)
        errs = blp_fetch.validate_payload({"requests": [
            {"kind": "intraday", "securities": ["A"], "fields": ["F"]}]})
        self.assertTrue(any("unknown request kind" in e for e in errs), errs)
        errs = blp_fetch.validate_payload({"requests": [
            {"kind": "history", "securities": [], "fields": [],
             "start": "20260814", "end": "20260814"}]})
        self.assertTrue(any("no securities" in e for e in errs), errs)
        self.assertTrue(any("no fields" in e for e in errs), errs)


class HelperTests(unittest.TestCase):
    def test_observation_typing(self):
        # Numbers -> num, everything else -> text. The sidecar never consults
        # field_def.value_kind; Rust owns that check.
        self.assertEqual(blp_fetch.observation("S", "F", "d", 1.5)["num"], 1.5)
        self.assertEqual(blp_fetch.observation("S", "F", "d", 7)["num"], 7.0)
        self.assertEqual(blp_fetch.observation("S", "F", "d", "X")["text"], "X")
        # bool is an int subclass in Python -- must not become a number
        self.assertEqual(blp_fetch.observation("S", "F", "d", True)["text"], "true")

    def test_security_error_classification(self):
        self.assertEqual(
            blp_fetch.classify_security_error({"category": "BAD_SEC"}),
            "invalid_security")
        self.assertEqual(
            blp_fetch.classify_security_error({"category": "NOT_ENTITLED"}),
            "not_entitled")
        self.assertEqual(
            blp_fetch.classify_security_error(
                {"category": "BAD_SEC", "subcategory": "NOT_ENTITLED"}),
            "not_entitled")


class _StubElement:
    """Records appendValue/appendElement/setElement calls; never touches blpapi."""

    def __init__(self):
        self.values = []

    def appendValue(self, v):
        self.values.append(v)

    def appendElement(self):
        e = _StubElement()
        self.values.append(e)
        return e

    def setElement(self, name, value):
        setattr(self, name, value)


class _StubRequest:
    """Records every request.set(field, value) call so a test can assert on it."""

    def __init__(self, name):
        self.name = name
        self.sets = {}
        self._elements = {}

    def set(self, field, value):
        self.sets[field] = value

    def getElement(self, name):
        return self._elements.setdefault(name, _StubElement())


class _StubService:
    def createRequest(self, name):
        return _StubRequest(name)


ADJUSTMENT_FLAGS = (
    "adjustmentNormal", "adjustmentAbnormal", "adjustmentSplit", "adjustmentFollowDPDF")


class BuildRequestTests(unittest.TestCase):
    """P0 3.1: without these four flags a stored price follows the Terminal's
    DPDF<GO> setting and is not reproducible. This is the task's highest-value
    four lines -- deleting them breaks nothing else, so they need their own
    test rather than relying on incidental coverage elsewhere.
    """

    def test_history_request_forces_all_four_adjustment_flags_false(self):
        spec = {"kind": "history", "securities": ["AAPL US Equity"], "fields": ["PX_LAST"],
                "start": "20260801", "end": "20260801"}
        req = blp_fetch.build_request(None, _StubService(), spec)
        for flag in ADJUSTMENT_FLAGS:
            self.assertIn(flag, req.sets, f"{flag} was never set on the history request")
            self.assertIs(req.sets[flag], False, f"{flag} must be exactly False")

    def test_reference_request_does_not_set_adjustment_flags(self):
        # Adjustment only means anything for a time series; a reference (BDP)
        # request has no history to adjust.
        spec = {"kind": "reference", "securities": ["AAPL US Equity"], "fields": ["NAME"],
                "obs_date": "2026-08-01"}
        req = blp_fetch.build_request(None, _StubService(), spec)
        for flag in ADJUSTMENT_FLAGS:
            self.assertNotIn(flag, req.sets)


class PeriodicityRequestTests(unittest.TestCase):
    """Spec 11.3 / probe F3: periodicity passes through to the wire, and the
    NIL-fill pair is set only when the effective periodicity is DAILY (F3:
    the fill options are accepted but inert under MONTHLY, so setting them
    only for DAILY keeps the contract explicit). Rust does not send
    "periodicity" yet (Task 3) -- absent key must reproduce today's exact
    request, byte-comparable.
    """

    def test_no_periodicity_key_defaults_to_daily_with_nil_pair_set(self):
        spec = {"kind": "history", "securities": ["AAPL US Equity"], "fields": ["PX_LAST"],
                "start": "20260801", "end": "20260801"}
        req = blp_fetch.build_request(None, _StubService(), spec)
        self.assertEqual(req.sets["periodicitySelection"], "DAILY")
        self.assertEqual(req.sets["nonTradingDayFillOption"], "NON_TRADING_WEEKDAYS")
        self.assertEqual(req.sets["nonTradingDayFillMethod"], "NIL_VALUE")

    def test_explicit_daily_periodicity_also_sets_nil_pair(self):
        spec = {"kind": "history", "securities": ["AAPL US Equity"], "fields": ["PX_LAST"],
                "start": "20260801", "end": "20260801", "periodicity": "DAILY"}
        req = blp_fetch.build_request(None, _StubService(), spec)
        self.assertEqual(req.sets["periodicitySelection"], "DAILY")
        self.assertEqual(req.sets["nonTradingDayFillOption"], "NON_TRADING_WEEKDAYS")
        self.assertEqual(req.sets["nonTradingDayFillMethod"], "NIL_VALUE")

    def test_monthly_periodicity_sets_selection_and_omits_nil_pair(self):
        spec = {"kind": "history", "securities": ["SPX Index"], "fields": ["PX_LAST"],
                "start": "20260101", "end": "20260831", "periodicity": "MONTHLY"}
        req = blp_fetch.build_request(None, _StubService(), spec)
        self.assertEqual(req.sets["periodicitySelection"], "MONTHLY")
        self.assertNotIn("nonTradingDayFillOption", req.sets)
        self.assertNotIn("nonTradingDayFillMethod", req.sets)

    def test_monthly_periodicity_still_sets_adjustment_flags(self):
        # Adjustment-flag block is unchanged for all periodicities (spec 11.3).
        spec = {"kind": "history", "securities": ["SPX Index"], "fields": ["PX_LAST"],
                "start": "20260101", "end": "20260831", "periodicity": "MONTHLY"}
        req = blp_fetch.build_request(None, _StubService(), spec)
        for flag in ADJUSTMENT_FLAGS:
            self.assertIn(flag, req.sets)
            self.assertIs(req.sets[flag], False)

    def test_weekly_and_quarterly_also_omit_nil_pair(self):
        for periodicity in ("WEEKLY", "QUARTERLY"):
            spec = {"kind": "history", "securities": ["SPX Index"], "fields": ["PX_LAST"],
                    "start": "20260101", "end": "20260831", "periodicity": periodicity}
            req = blp_fetch.build_request(None, _StubService(), spec)
            self.assertEqual(req.sets["periodicitySelection"], periodicity)
            self.assertNotIn("nonTradingDayFillOption", req.sets)
            self.assertNotIn("nonTradingDayFillMethod", req.sets)


class PeriodicityValidationTests(unittest.TestCase):
    """Bloomberg launders bad enum values into empty results, so an unknown
    periodicity string must be a loud validation error naming the spec
    index -- never silently accepted or defaulted.
    """

    def test_lowercase_periodicity_is_rejected(self):
        errs = blp_fetch.validate_payload({"requests": [
            {"kind": "history", "securities": ["A"], "fields": ["F"],
             "start": "20260801", "end": "20260801", "periodicity": "weekly"}]})
        self.assertTrue(any("requests[0]" in e and "periodicity" in e for e in errs), errs)

    def test_garbage_periodicity_is_rejected(self):
        errs = blp_fetch.validate_payload({"requests": [
            {"kind": "history", "securities": ["A"], "fields": ["F"],
             "start": "20260801", "end": "20260801", "periodicity": "FORTNIGHTLY"}]})
        self.assertTrue(any("requests[0]" in e and "periodicity" in e for e in errs), errs)

    def test_valid_periodicities_pass_validation(self):
        for periodicity in ("DAILY", "WEEKLY", "MONTHLY", "QUARTERLY"):
            errs = blp_fetch.validate_payload({"requests": [
                {"kind": "history", "securities": ["A"], "fields": ["F"],
                 "start": "20260801", "end": "20260801", "periodicity": periodicity}]})
            self.assertEqual(errs, [], (periodicity, errs))

    def test_absent_periodicity_key_passes_validation(self):
        errs = blp_fetch.validate_payload({"requests": [
            {"kind": "history", "securities": ["A"], "fields": ["F"],
             "start": "20260801", "end": "20260801"}]})
        self.assertEqual(errs, [])


class RealEodTests(unittest.TestCase):
    """Live capture: 2 securities x 2 history fields + NAME, obs_date 2026-08-17."""

    def setUp(self):
        (self.obs, self.probs, self.bulk_rows, self.list_results,
         self.fatal) = blp_fetch.parse_capture(load("real_eod.json"))
        self.got = by_key(self.obs)

    def test_clean_run_has_no_problems(self):
        self.assertIsNone(self.fatal)
        self.assertEqual(self.probs, [])
        self.assertEqual(len(self.obs), 6)

    def test_numeric_values_parsed(self):
        self.assertEqual(
            self.got[("AAPL US Equity", "PX_LAST", "2026-08-17")]["num"], 305.59)
        self.assertEqual(
            self.got[("SX5E Index", "PX_LAST", "2026-08-17")]["num"], 6530.45)

    def test_reference_text_stamped_with_request_obs_date(self):
        # Amendment A1: text fields have no history, so their live value is
        # stored under the run's previous-trading-day obs_date.
        row = self.got[("AAPL US Equity", "NAME", "2026-08-17")]
        self.assertEqual(row["text"], "APPLE INC")
        self.assertNotIn("num", row)

    def test_history_and_reference_agree_on_date(self):
        # Both request kinds must land on the same obs_date or the observation
        # primary key would split one asset-day across two rows.
        dates = {o["date"] for o in self.obs}
        self.assertEqual(dates, {"2026-08-17"})


class RealProblemTests(unittest.TestCase):
    def setUp(self):
        (self.obs, self.probs, self.bulk_rows, self.list_results,
         self.fatal) = blp_fetch.parse_capture(load("real_problems.json"))
        self.p = by_key(self.probs)

    def test_partial_run_still_returns_good_data(self):
        # One failing security must never poison the rest of the run.
        self.assertIsNone(self.fatal)
        self.assertTrue(self.obs)
        self.assertTrue(self.probs)

    def test_invalid_field_reported_as_field_not_applicable(self):
        row = self.p[("AAPL US Equity", "BOGUS_FIELD_XYZ", "2026-08-17")]
        self.assertEqual(row["code"], "field_not_applicable")
        self.assertIn("BAD_FLD", row["detail"])

    def test_field_exception_does_not_suppress_sibling_fields(self):
        got = by_key(self.obs)
        self.assertEqual(got[("AAPL US Equity", "NAME", "2026-08-17")]["text"],
                         "APPLE INC")

    def test_resolvable_security_with_no_data_is_no_data(self):
        # 'XXXX US Equity' resolves at Bloomberg but returned an empty
        # fieldData with NO securityError -- the same shape a holiday
        # produces. It is correctly `no_data`, not `invalid_security`.
        for f in ("PX_LAST", "PX_VOLUME"):
            self.assertEqual(
                self.p[("XXXX US Equity", f, "2026-08-17")]["code"], "no_data")


class RealBadSecurityTests(unittest.TestCase):
    """A genuinely unknown ticker DOES produce securityError, on both kinds."""

    def setUp(self):
        (self.obs, self.probs, self.bulk_rows, self.list_results,
         self.fatal) = blp_fetch.parse_capture(load("real_bad_security.json"))

    def test_no_observations_and_all_problems_are_invalid_security(self):
        self.assertIsNone(self.fatal)
        self.assertEqual(self.obs, [])
        self.assertEqual({p["code"] for p in self.probs}, {"invalid_security"})

    def test_both_request_kinds_report_it(self):
        # HistoricalDataRequest and ReferenceDataRequest use different message
        # shapes (securityData dict vs list) and different message text.
        self.assertEqual({p["field"] for p in self.probs}, {"PX_LAST", "NAME"})
        for p in self.probs:
            self.assertIn("Invalid Security", p["detail"])

    def test_reference_null_field_data_is_survivable(self):
        # Live capture has "fieldData": null (not {}) alongside securityError.
        raw = load("real_bad_security.json")
        ref = [c for c in raw["captured"] if c["request"]["kind"] == "reference"][0]
        self.assertIsNone(ref["messages"][0]["securityData"][0]["fieldData"])


class RealBackfillTests(unittest.TestCase):
    def test_multiday_rows_all_ingested_in_order(self):
        obs, probs, _bulk_rows, _list_results, fatal = blp_fetch.parse_capture(
            load("real_backfill.json"))
        self.assertIsNone(fatal)
        self.assertEqual(probs, [])
        self.assertEqual([o["date"] for o in obs],
                         ["2026-08-10", "2026-08-11", "2026-08-12",
                          "2026-08-13", "2026-08-14"])

    def test_multiday_gap_is_not_reported_as_no_data(self):
        # Over a range we cannot know which days should have existed, so
        # absence is a gap, not an issue -- consistent with the
        # no-holiday-calendar design. Only single-day (EOD) requests emit
        # no_data.
        capture = {"captured": [{
            "request": {"kind": "history", "securities": ["AAPL US Equity"],
                        "fields": ["PX_LAST"],
                        "start": "20260701", "end": "20260731"},
            "messages": [{"securityData": {
                "security": "AAPL US Equity", "fieldExceptions": [],
                "fieldData": []}}]}]}
        obs, probs, _bulk_rows, _list_results, fatal = blp_fetch.parse_capture(capture)
        self.assertIsNone(fatal)
        self.assertEqual((obs, probs), ([], []))


class EmptyResultTests(unittest.TestCase):
    """A response containing literally nothing means different things for
    different request kinds. history/reference/bulk_reference always name a
    security, so silence about all of them is structurally impossible for a
    well-formed request and stays a fault. instrument_list is a text search:
    "no security matches this text" is itself a normal answer, so an empty
    instrumentListRequest result must not be confused with a session fault."""

    def run_finish(self, capture):
        import contextlib
        import io
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = blp_fetch.finish(capture, 0.0)
        return code, json.loads(buf.getvalue())

    def test_an_empty_instrument_list_response_is_a_clean_success(self):
        capture = {"captured": [{
            "request": {"kind": "instrument_list", "query": "ZZZZNOPE",
                        "max_results": 20},
            "messages": [{"results": []}]}]}
        code, out = self.run_finish(capture)
        self.assertEqual(code, blp_fetch.EXIT_OK)
        self.assertEqual(out["status"], "ok")
        self.assertEqual(out["list_results"], [])

    def test_an_instrument_list_response_with_no_messages_at_all_is_also_ok(self):
        # Belt and suspenders: even if Bloomberg's response never produced a
        # message (as opposed to one message with an empty results array),
        # a legitimate zero-match search must not be reported as EXIT_SESSION.
        capture = {"captured": [{
            "request": {"kind": "instrument_list", "query": "ZZZZNOPE",
                        "max_results": 20},
            "messages": []}]}
        code, out = self.run_finish(capture)
        self.assertEqual(code, blp_fetch.EXIT_OK)
        self.assertEqual(out["status"], "ok")

    def test_an_empty_history_response_still_faults(self):
        # The check this test guards was written for exactly this case: a
        # history/reference/bulk_reference response with nothing in it at all
        # really does indicate something wrong upstream, and must keep
        # exiting EXIT_SESSION rather than being laundered into a success by
        # the instrument_list carve-out above.
        capture = {"captured": [{
            "request": {"kind": "history", "securities": ["AAPL US Equity"],
                        "fields": ["PX_LAST"], "start": "20260701",
                        "end": "20260701"},
            "messages": []}]}
        code, out = self.run_finish(capture)
        self.assertEqual(code, blp_fetch.EXIT_SESSION)
        self.assertEqual(out["status"], "empty")


class NilFillTests(unittest.TestCase):
    """Task 5 / P0 8: with nonTradingDayFillOption=NON_TRADING_WEEKDAYS and
    nonTradingDayFillMethod=NIL_VALUE, Bloomberg returns an explicit dated
    fieldData row for a holiday instead of omitting the day entirely -- but
    the row carries none of the requested field values. Before this, such a
    row was silently dropped, so a genuine exchange holiday inside a backfill
    range was indistinguishable from a gap the pipeline never asked about.
    Now it must surface as a dated `no_data` problem so ingest Rule A can mark
    it a non-trading day rather than a gap to keep retrying forever.
    """

    def test_nil_fill_row_is_reported_as_a_non_trading_day_problem(self):
        capture = {"captured": [{
            "request": {"kind": "history", "securities": ["AAPL US Equity"],
                        "fields": ["PX_LAST"], "start": "20260814", "end": "20260817"},
            "messages": [{"securityData": {
                "security": "AAPL US Equity", "eidData": [], "sequenceNumber": 0,
                "fieldExceptions": [],
                "fieldData": [
                    {"date": "2026-08-14", "PX_LAST": 305.93},
                    {"date": "2026-08-17"},
                ]}}]}]}
        obs, probs, _bulk_rows, _list_results, fatal = blp_fetch.parse_capture(capture)
        self.assertIsNone(fatal)
        self.assertEqual(len(obs), 1)
        self.assertEqual(obs[0], {"security": "AAPL US Equity", "field": "PX_LAST",
                                  "date": "2026-08-14", "num": 305.93})
        self.assertEqual(probs, [{"security": "AAPL US Equity", "field": None,
                                  "date": "2026-08-17", "code": "no_data",
                                  "detail": "non-trading day (NIL fill)"}])


class LiveWireFixtureTests(unittest.TestCase):
    """Pinned live wire captures from the 2026-08-22 probe (spec F1/F6),
    committed as `--raw-out` captures at src-tauri/scripts/fixtures/. These
    are raw captures against today's DAILY + NIL-fill request (no
    "periodicity" key), so they pin today's parsing behaviour unchanged by
    this task and guard against any future regression to it.
    """

    def test_multiasset_nilfill_capture_reproduces_45_obs_4_problems(self):
        # F1: SPX/AAPL/CL1 each NIL-fill on 2026-07-03 (US Independence Day
        # observed); EUR/XAU print real values that day; the bad bond ticker
        # is invalid_security. 45 observations total, 4 problems.
        obs, probs, _bulk, _lst, fatal = blp_fetch.parse_capture(
            load_live("live-2026-08-22-nilfill-multiasset-history.json"))
        self.assertIsNone(fatal)
        self.assertEqual(len(obs), 45)
        self.assertEqual(len(probs), 4)

        no_data = sorted((p["security"], p["date"]) for p in probs
                         if p["code"] == "no_data")
        self.assertEqual(no_data, [
            ("AAPL UW Equity", "2026-07-03"),
            ("CL1 Comdty", "2026-07-03"),
            ("SPX Index", "2026-07-03"),
        ])

        invalid = [p for p in probs if p["code"] == "invalid_security"]
        self.assertEqual(len(invalid), 1)
        self.assertEqual(invalid[0]["security"], "T 2 3/8 05/15/31 Govt")

    def test_bond_allnil_capture_reproduces_0_obs_25_problems(self):
        # F6: individual govt bond tickers have no historical PX_LAST
        # entitlement on this licence -- NIL for every weekday, every
        # addressing form. 0 observations, 25 problems (24 no_data + 1
        # invalid_security for the coupon-style ticker).
        obs, probs, _bulk, _lst, fatal = blp_fetch.parse_capture(
            load_live("live-2026-08-22-bond-allnil-history.json"))
        self.assertIsNone(fatal)
        self.assertEqual(obs, [])
        self.assertEqual(len(probs), 25)
        codes = Counter(p["code"] for p in probs)
        self.assertEqual(codes["no_data"], 24)
        self.assertEqual(codes["invalid_security"], 1)


class FatalTests(unittest.TestCase):
    def test_bad_date_is_rejected_before_it_can_be_mistaken_for_a_holiday(self):
        # Live capture of start=end=20261301. Bloomberg returned an empty
        # fieldData and NO error, so without validation this would have become
        # a `no_data` issue dated 2026-13-01 on an exit-0 run.
        obs, probs, _bulk_rows, _list_results, fatal = blp_fetch.parse_capture(
            load("real_bad_date.json"))
        self.assertIsNotNone(fatal)
        self.assertIn("invalid start date", fatal)
        self.assertEqual((obs, probs), ([], []))

    def test_response_error_is_fatal_not_per_cell(self):
        # A request-level failure is not attributable to any one security, so
        # it must fail the run rather than becoming warnings.
        obs, probs, _bulk_rows, _list_results, fatal = blp_fetch.parse_capture(
            load("response_error.json"))
        self.assertIsNotNone(fatal)
        self.assertIn("Invalid start date", fatal)

    def test_unknown_request_kind_is_fatal(self):
        obs, probs, _bulk_rows, _list_results, fatal = blp_fetch.parse_capture(
            {"captured": [{"request": {"kind": "intraday"},
                           "messages": [{"securityData": {}}]}]})
        self.assertIsNotNone(fatal)
        self.assertIn("unknown request kind", fatal)


if __name__ == "__main__":
    unittest.main(verbosity=2)
