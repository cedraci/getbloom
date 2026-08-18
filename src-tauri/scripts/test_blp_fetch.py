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

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import blp_fetch  # noqa: E402

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                        "..", "tests", "fixtures", "blpapi")


def load(name):
    with open(os.path.join(FIXTURES, name), encoding="utf-8") as fh:
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


class RealEodTests(unittest.TestCase):
    """Live capture: 2 securities x 2 history fields + NAME, obs_date 2026-08-17."""

    def setUp(self):
        self.obs, self.probs, self.fatal = blp_fetch.parse_capture(
            load("real_eod.json"))
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
        self.obs, self.probs, self.fatal = blp_fetch.parse_capture(
            load("real_problems.json"))
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
        self.obs, self.probs, self.fatal = blp_fetch.parse_capture(
            load("real_bad_security.json"))

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
        obs, probs, fatal = blp_fetch.parse_capture(load("real_backfill.json"))
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
        obs, probs, fatal = blp_fetch.parse_capture(capture)
        self.assertIsNone(fatal)
        self.assertEqual((obs, probs), ([], []))


class FatalTests(unittest.TestCase):
    def test_bad_date_is_rejected_before_it_can_be_mistaken_for_a_holiday(self):
        # Live capture of start=end=20261301. Bloomberg returned an empty
        # fieldData and NO error, so without validation this would have become
        # a `no_data` issue dated 2026-13-01 on an exit-0 run.
        obs, probs, fatal = blp_fetch.parse_capture(load("real_bad_date.json"))
        self.assertIsNotNone(fatal)
        self.assertIn("invalid start date", fatal)
        self.assertEqual((obs, probs), ([], []))

    def test_response_error_is_fatal_not_per_cell(self):
        # A request-level failure is not attributable to any one security, so
        # it must fail the run rather than becoming warnings.
        obs, probs, fatal = blp_fetch.parse_capture(load("response_error.json"))
        self.assertIsNotNone(fatal)
        self.assertIn("Invalid start date", fatal)

    def test_unknown_request_kind_is_fatal(self):
        obs, probs, fatal = blp_fetch.parse_capture(
            {"captured": [{"request": {"kind": "intraday"},
                           "messages": [{"securityData": {}}]}]})
        self.assertIsNotNone(fatal)
        self.assertIn("unknown request kind", fatal)


if __name__ == "__main__":
    unittest.main(verbosity=2)
