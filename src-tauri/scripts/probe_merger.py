#!/usr/bin/env python
"""Live probe: fund-merger detection and terms through the Desktop API.

Purpose (P0 discipline: field names are never guessed into production code;
they are measured here first, against a real past fund merger, and only what
this probe proves goes into the design):

  Q1  Which request returns fund-merger corporate-action records -- and do
      they carry an Action ID? Two routes are tried:
        a) the existing 2-field corp-actions call with a WIDENED
           CORPORATE_ACTIONS_FILTER (if this works, detection is free:
           the filter is an override, not a field, so no extra hits), and
        b) candidate bulk fields that may hold the M&A / corp-action table.
  Q2  Does `"<ActionID> Action"` work as a ReferenceDataRequest security
      string, and which fields (TARGET_SHARES_RATIO per Bloomberg support,
      plus candidates for acquirer / effective date) come back populated?
  Q3  Which DIRECTION is TARGET_SHARES_RATIO? The probe pulls raw prices for
      both funds around the effective date and prints the implied ratio both
      ways, so the answer is read off, not assumed. stitch.rs multiplies
      predecessor values by the ratio -- an inverted convention would corrupt
      every stitched series, so this must be pinned before any code changes.

Every step is optional, so the probe can be run in passes:

  1st pass (Q1):    python probe_merger.py --target "OLD FUND LN Equity"
  If Q1 finds no Action ID, read it off the Terminal (CACS <GO> on the target
  fund, open the merger record in CACX <GO>), then:
  2nd pass (Q2+Q3): python probe_merger.py --target "OLD FUND LN Equity" \
                        --acquirer "NEW FUND LN Equity" \
                        --action-id 123456789 --effective 2024-05-17

The full raw capture is always written (default probe_merger_raw.json, same
shape as blp_fetch.py --raw-out) -- that file is the evidence to check in,
exactly like the P0 captures.

Cost: every candidate field is 1 hit (1 security x 1 field); a full run of
all steps is roughly 25-35 hits. These hits are NOT recorded in the app's
hit_ledger -- this script never touches the database -- so count the day's
budget accordingly.

This is a diagnostic, run by a human at a Terminal. Unlike blp_fetch.py,
stdout is a report for the human, not a protocol.
"""

import argparse
import datetime as dt
import json
import re
import sys
import time

import blp_fetch as bf

# ---------------------------------------------------------------------------
# Candidates. ALL of these are guesses to be tested -- that is this file's
# entire job. Nothing here may be copied into production without a populated
# answer in the raw capture. Extend from FLDS <GO> via --detect-fields /
# --terms-fields if these all come back invalid.
# ---------------------------------------------------------------------------

# The three filter tokens the shipping code already uses (P0-verified):
FILTER_BASE = ["NORMAL_CASH", "ABNORMAL_CASH", "CAPITAL_CHANGE"]
# Candidate extra tokens that might make merger records appear:
FILTER_CANDIDATES = ["ACQUIS", "DIVEST", "MERGER", "STOCK_CHG", "FUND_MERGER"]

# Candidate bulk (table) fields that might hold corp-action / M&A records
# with Action IDs:
DETECT_FIELD_CANDIDATES = [
    "MERGERS_AND_ACQUISITIONS",
    "CORP_ACTION_INFO",
    "MERGER_INFO",
    "FUND_MERGER_INFO",
    "EQY_CORP_ACTIONS",
]

# Candidate fields for the "<ActionID> Action" reference request.
# TARGET_SHARES_RATIO is the one Bloomberg support named; the rest are
# candidates for the acquirer identity and the effective date.
TERMS_FIELD_CANDIDATES = [
    "TARGET_SHARES_RATIO",
    "ACTION_TYPE",
    "CORP_ACTION_TYPE",
    "EFFECTIVE_DATE",
    "CA_EFFECTIVE_DT",
    "ANNOUNCE_DT",
    "ACQUIRER_TICKER",
    "ACQUIRER_NAME",
    "TARGET_TICKER",
]

ACTION_ID_KEY = re.compile(r"action.*id|id.*action", re.IGNORECASE)


def hr(title):
    print(f"\n{'=' * 74}\n{title}\n{'=' * 74}")


def send_one(blpapi, session, spec, deadline, captured):
    """Send one request; capture verbatim; never let one failure end the run."""
    service = session.getService(bf.REFDATA_SERVICE)
    n_hits = len(spec.get("securities", [])) * len(spec.get("fields", []))
    print(f"\n-> {spec['kind']}: securities={spec.get('securities')} "
          f"fields={spec.get('fields')} (~{n_hits} hits)")
    for ov in spec.get("overrides") or []:
        print(f"   override {ov['fieldId']} = {ov['value']}")
    try:
        req = bf.build_request(blpapi, service, spec)
        messages = bf.send_and_drain(blpapi, session, req, deadline)
    except (bf.SessionError, bf.TimeoutError_) as e:
        print(f"   [FAIL] {type(e).__name__}: {e}")
        captured.append({"request": spec, "messages": [],
                         "probe_error": str(e)})
        return None, n_hits
    captured.append({"request": spec, "messages": messages})
    for msg in messages:
        err = msg.get("responseError")
        if err:
            print(f"   [responseError] {err.get('category', '')}: "
                  f"{err.get('message', '')}")
    return messages, n_hits


def report_security_data(messages):
    """Print fieldData / fieldExceptions / securityError verbatim but compact."""
    for msg in messages or []:
        for sec in bf.as_list(msg.get("securityData")):
            security = sec.get("security")
            err = sec.get("securityError")
            if err:
                print(f"   [securityError] {security}: "
                      f"{err.get('category', '')}/{err.get('subcategory', '')} "
                      f"{err.get('message', '')}")
                continue
            for fe in bf.as_list(sec.get("fieldExceptions")):
                info = fe.get("errorInfo") or {}
                print(f"   [field invalid] {security} {fe.get('fieldId')}: "
                      f"{info.get('category', '')}/{info.get('subcategory', '')} "
                      f"{info.get('message', '')}")
            fdata = sec.get("fieldData")
            if fdata is None:
                continue
            if isinstance(fdata, dict):
                for k, v in fdata.items():
                    if isinstance(v, list):
                        print(f"   [table] {security} {k}: {len(v)} row(s)")
                        for row in v:
                            print(f"           {json.dumps(row, default=str)}")
                    else:
                        print(f"   [value] {security} {k} = {v!r}")


def find_action_ids(messages):
    """Scan every returned table row for a key that looks like an Action ID."""
    found = []
    for msg in messages or []:
        for sec in bf.as_list(msg.get("securityData")):
            fdata = sec.get("fieldData")
            if not isinstance(fdata, dict):
                continue
            for field, value in fdata.items():
                if not isinstance(value, list):
                    continue
                for row in value:
                    if not isinstance(row, dict):
                        continue
                    for k, v in row.items():
                        if ACTION_ID_KEY.search(k):
                            found.append((field, k, v))
    return found


# ---------------------------------------------------------------------------
# Steps
# ---------------------------------------------------------------------------

def step_q1_detection(blpapi, session, args, deadline, captured):
    hr(f"Q1 -- merger detection for target {args.target}")
    total = 0

    # a) The free route: the exact call corp_actions.rs makes today, with the
    #    filter widened. First all candidate tokens at once (1 request); if
    #    Bloomberg rejects the combined value outright, once per token.
    combined = "|".join(FILTER_BASE + args.filter_candidates)
    spec = {"kind": "bulk_reference", "securities": [args.target],
            "fields": ["EQY_DVD_ADJUST_FACT", "DVD_HIST_ALL_WITH_AMT_STATUS"],
            "overrides": [{"fieldId": "CORPORATE_ACTIONS_FILTER",
                           "value": combined}]}
    messages, hits = send_one(blpapi, session, spec, deadline, captured)
    total += hits
    report_security_data(messages)
    rejected = any(m.get("responseError") for m in messages or [])
    if rejected:
        print("\n   combined filter value rejected; trying tokens one at a time")
        for tok in args.filter_candidates:
            spec_tok = dict(spec)
            spec_tok["overrides"] = [{"fieldId": "CORPORATE_ACTIONS_FILTER",
                                      "value": "|".join(FILTER_BASE + [tok])}]
            messages, hits = send_one(blpapi, session, spec_tok, deadline,
                                      captured)
            total += hits
            report_security_data(messages)

    # b) Candidate bulk fields. Invalid names come back as fieldExceptions,
    #    which is exactly the evidence we want recorded.
    spec = {"kind": "bulk_reference", "securities": [args.target],
            "fields": args.detect_fields}
    messages, hits = send_one(blpapi, session, spec, deadline, captured)
    total += hits
    report_security_data(messages)

    ids = find_action_ids(
        [m for item in captured for m in item.get("messages", [])])
    if ids:
        print("\n   *** Action-ID-shaped keys found: ***")
        for field, key, value in ids:
            print(f"       {field}.{key} = {value!r}")
        print("   Re-run with --action-id <value> to probe Q2/Q3.")
    else:
        print("\n   No Action-ID-shaped key in any returned table. Read the "
              "Action ID off CACS <GO> / CACX <GO> and re-run with "
              "--action-id.")
    return total


def step_q2_terms(blpapi, session, args, deadline, captured):
    security = f"{args.action_id} Action"
    hr(f"Q2 -- terms at the Action-ID level: \"{security}\"")
    spec = {"kind": "reference", "securities": [security],
            "fields": args.terms_fields,
            "obs_date": dt.date.today().isoformat()}
    messages, hits = send_one(blpapi, session, spec, deadline, captured)
    report_security_data(messages)
    ratio = None
    for msg in messages or []:
        for sec in bf.as_list(msg.get("securityData")):
            fdata = sec.get("fieldData") or {}
            if isinstance(fdata, dict) and \
                    fdata.get("TARGET_SHARES_RATIO") is not None:
                ratio = fdata["TARGET_SHARES_RATIO"]
    if ratio is not None:
        print(f"\n   *** TARGET_SHARES_RATIO = {ratio!r} ***")
    else:
        print("\n   TARGET_SHARES_RATIO not populated. Per Bloomberg support "
              "it fills on/shortly after the effective date -- if that date "
              "is past, check the CACX <GO> record: blank there means blank "
              "via API too.")
    return hits, ratio


def step_q3_direction(blpapi, session, args, deadline, captured, ratio):
    hr(f"Q3 -- ratio direction via raw prices around {args.effective}")
    eff = dt.date.fromisoformat(args.effective)
    start = (eff - dt.timedelta(days=14)).strftime("%Y%m%d")
    end = (eff + dt.timedelta(days=14)).strftime("%Y%m%d")
    total = 0
    series = {}
    for security in (args.target, args.acquirer):
        spec = {"kind": "history", "securities": [security],
                "fields": [args.price_field], "start": start, "end": end}
        messages, hits = send_one(blpapi, session, spec, deadline, captured)
        total += hits
        rows = []
        for msg in messages or []:
            sec = msg.get("securityData") or {}
            if isinstance(sec, list):
                sec = sec[0] if sec else {}
            for row in bf.as_list(sec.get("fieldData")):
                d = bf.iso_date(row.get("date"))
                if d is not None and args.price_field in row:
                    rows.append((dt.date.fromisoformat(d),
                                 float(row[args.price_field])))
        rows.sort()
        series[security] = rows
        for d, v in rows:
            print(f"   {security} {d} {args.price_field} = {v}")

    pred = [(d, v) for d, v in series.get(args.target, []) if d < eff]
    succ = [(d, v) for d, v in series.get(args.acquirer, []) if d >= eff]
    if not pred or not succ:
        print("\n   [FAIL] need at least one target price before the "
              "effective date and one acquirer price at/after it")
        return total
    p_date, p = pred[-1]
    s_date, s = succ[0]
    print(f"\n   last target value before effective:    {p} ({p_date})")
    print(f"   first acquirer value at/after:         {s} ({s_date})")
    print(f"   price-continuity ratio succ/pred = {s / p:.6f}  "
          "(what stitch.rs derives today)")
    print(f"   inverse                pred/succ = {p / s:.6f}")
    if ratio is not None:
        try:
            r = float(ratio)
            print(f"\n   TARGET_SHARES_RATIO = {r}")
            # If 1 target share converts into r acquirer shares, then value
            # continuity says price_target ~= r * price_acquirer, i.e. the
            # splice multiplier stitch.rs needs (succ/pred units) is 1/r.
            print(f"   -> if 1 target share -> {r} acquirer shares, the "
                  f"splice multiplier is 1/r = {1 / r:.6f}; compare against "
                  f"{s / p:.6f} above to confirm or refute the direction.")
        except (TypeError, ValueError):
            print(f"\n   TARGET_SHARES_RATIO {ratio!r} is not numeric -- "
                  "record the shape, it changes the parsing design.")
    return total


def main():
    ap = argparse.ArgumentParser(
        description="Live probe for fund-merger detection and terms")
    ap.add_argument("--target", required=True,
                    help='the merged-away fund, e.g. "OLD FUND LN Equity"')
    ap.add_argument("--acquirer",
                    help='the surviving fund (enables Q3 with --effective)')
    ap.add_argument("--action-id",
                    help="numeric Action ID from Q1 or CACX <GO> (enables Q2)")
    ap.add_argument("--effective",
                    help="merger effective date YYYY-MM-DD (enables Q3)")
    ap.add_argument("--price-field", default="PX_LAST",
                    help="price/NAV field for the Q3 direction check")
    ap.add_argument("--detect-fields", nargs="*",
                    default=DETECT_FIELD_CANDIDATES,
                    help="candidate bulk fields for Q1 (from FLDS <GO>)")
    ap.add_argument("--terms-fields", nargs="*",
                    default=TERMS_FIELD_CANDIDATES,
                    help="candidate fields for the Action-ID request (Q2)")
    ap.add_argument("--filter-candidates", nargs="*",
                    default=FILTER_CANDIDATES,
                    help="extra CORPORATE_ACTIONS_FILTER tokens to try (Q1)")
    ap.add_argument("--raw-out", default="probe_merger_raw.json",
                    help="raw capture file (the evidence; same shape as "
                         "blp_fetch.py --raw-out)")
    ap.add_argument("--host", default=bf.DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=bf.DEFAULT_PORT)
    ap.add_argument("--timeout", type=float, default=120.0)
    args = ap.parse_args()

    if args.effective:
        try:
            dt.date.fromisoformat(args.effective)
        except ValueError:
            print(f"--effective {args.effective!r} is not YYYY-MM-DD")
            return bf.EXIT_BADINPUT

    print(f"fund-merger probe -> {args.host}:{args.port}")
    try:
        blpapi = bf.import_blpapi()
        session = bf.open_session(blpapi, args.host, args.port)
    except bf.SessionError as e:
        print(f"[FAIL] {e}")
        return bf.EXIT_SESSION
    print(f"[ok] session started, {bf.REFDATA_SERVICE} open")

    deadline = time.monotonic() + args.timeout
    captured = []
    total_hits = 0
    ratio = None
    try:
        total_hits += step_q1_detection(blpapi, session, args, deadline,
                                        captured)
        if args.action_id:
            hits, ratio = step_q2_terms(blpapi, session, args, deadline,
                                        captured)
            total_hits += hits
        else:
            print("\n(skipping Q2: no --action-id)")
        if args.acquirer and args.effective:
            total_hits += step_q3_direction(blpapi, session, args, deadline,
                                            captured, ratio)
        else:
            print("(skipping Q3: needs both --acquirer and --effective)")
    finally:
        try:
            session.stop()
        except Exception:
            pass

    with open(args.raw_out, "w", encoding="utf-8") as fh:
        json.dump({"probe": "fund_merger", "args": vars(args),
                   "captured": captured}, fh, indent=1, default=str)
    hr("done")
    print(f"~{total_hits} hits spent (NOT in the app's hit_ledger -- budget "
          f"accordingly). Raw capture: {args.raw_out}")
    print("Bring that file back: populated answers go into the design, "
          "everything else stays out.")
    return bf.EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
