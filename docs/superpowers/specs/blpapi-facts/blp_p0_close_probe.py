#!/usr/bin/env python
"""THROWAWAY SPIKE — close the remaining P0 open items.

Four questions, one session:

  1. EQY_DVD_ADJUST_FACT's "Adjustment Factor Operator Type" and
     "Adjustment Factor Flag" semantics. Decoded against AAPL, whose split
     history is public and unambiguous, so the observed factors can be checked
     against ratios we already know rather than guessed at.
  2. The full CA_MA_* membership, live -- the committed field dump is partial
     (8637 mnemonics) and does NOT contain CA_MA_COMPLETE_DT, which the P1
     design cites. Absence from a partial dump proves nothing either way.
  3. SIMP_SEC_STATUS's value domain, observed across a deliberately mixed
     basket rather than assumed.
  4. Every remaining mnemonic the P1 spec and plan name, confirmed to exist.

Read-only. Roughly a dozen requests.
"""
import json
import os
import sys
import time

HOST, PORT = "localhost", 8194

# Every mnemonic P1 relies on that P0 has not already confirmed by name, plus
# the ones the design cites for instrument_link evidence.
CONFIRM = [
    "CA_MA_COMPLETE_DT",
    "REVERSE_MERGER_COMPLETION_DATE",
    "FUND_SHARE_CLASS_CLOSURE_DATE",
    "SIMP_SEC_STATUS",
    "INACTIVE_DATE",
    "LISTING_DATE",
    "ID_BB_UNIQUE",
    "ID_BB_GLOBAL_SHARE_CLASS_LEVEL",
    "FUND_SHR_CLASS_DESG",
    "SHARE_CLASS_TYPE",
    "FUND_TYP",
    "EQY_DVD_ADJUST_FACT",
    "HISTORICAL_IDS_TIME_RANGE",
    "HISTORICAL_STARTING_IDENTIFIER",
    "HISTORICAL_ID_TM_RANGE_START_DT",
    "CORPORATE_ACTIONS_FILTER",
]

SEARCHES = ["CA_MA", "merger completion", "security status", "trading status"]

# A deliberately mixed basket: a live large cap, a live fund share class, an
# index, a bond, and one security whose status should NOT be "active".
STATUS_BASKET = [
    "AAPL US Equity",
    "META US Equity",
    "VFIAX US Equity",
    "SX5E Index",
    "FB US Equity",       # the old ticker; may or may not still resolve
    "TWTR US Equity",     # taken private 2022 -- the interesting case
]


def log(m):
    print(m, file=sys.stderr, flush=True)


def drain(blpapi, s, deadline=None):
    deadline = deadline or time.monotonic() + 120
    out = []
    while True:
        rem = deadline - time.monotonic()
        if rem <= 0:
            raise TimeoutError("deadline")
        ev = s.nextEvent(int(min(rem, 5.0) * 1000))
        et = ev.eventType()
        if et in (blpapi.Event.PARTIAL_RESPONSE, blpapi.Event.RESPONSE):
            for m in ev:
                out.append(m.toPy() if hasattr(m, "toPy") else str(m))
            if et == blpapi.Event.RESPONSE:
                return out


def field_infos(msgs):
    acc = {}

    def w(n):
        if isinstance(n, dict):
            fi = n.get("fieldInfo")
            if fi and fi.get("mnemonic"):
                acc[fi["mnemonic"]] = fi
            for v in n.values():
                w(v)
        elif isinstance(n, list):
            for v in n:
                w(v)

    w(msgs)
    return acc


def field_data(msgs, field):
    """Pull one field's value per security out of a ReferenceDataRequest reply."""
    out = {}

    def w(n):
        if isinstance(n, dict):
            for sd in n.get("securityData", []) or []:
                if isinstance(sd, dict):
                    sec = sd.get("security")
                    if sd.get("securityError"):
                        out[sec] = {"__error__": sd["securityError"].get("subcategory")}
                        continue
                    fd = sd.get("fieldData") or {}
                    if field in fd:
                        out[sec] = fd[field]
            for v in n.values():
                w(v)
        elif isinstance(n, list):
            for v in n:
                w(v)

    w(msgs)
    return out


def main():
    os.makedirs("probe_out", exist_ok=True)
    import blpapi

    o = blpapi.SessionOptions()
    o.setServerHost(HOST)
    o.setServerPort(PORT)
    s = blpapi.Session(o)
    if not s.start():
        log("session.start() failed -- is the Terminal running and logged in?")
        return 2
    for u in ("//blp/refdata", "//blp/apiflds"):
        if not s.openService(u):
            log(f"openService({u}) failed")
            return 2
    ref, flds = s.getService("//blp/refdata"), s.getService("//blp/apiflds")
    R = {}

    # ---- 1. do these mnemonics exist, and what do they document?
    r = flds.createRequest("FieldInfoRequest")
    for m in CONFIRM:
        r.getElement("id").appendValue(m)
    r.set("returnFieldDocumentation", True)
    s.sendRequest(r)
    info = field_infos(drain(blpapi, s))
    R["field_info"] = info
    missing = [m for m in CONFIRM if m not in info]
    log(f"1. FieldInfoRequest: {len(info)}/{len(CONFIRM)} confirmed")
    for m in missing:
        log(f"   MISSING: {m}")

    # ---- 2. the CA_MA family and the status fields, live
    R["searches"] = {}
    for spec in SEARCHES:
        r = flds.createRequest("FieldSearchRequest")
        r.set("searchSpec", spec)
        r.set("returnFieldDocumentation", False)
        s.sendRequest(r)
        got = field_infos(drain(blpapi, s))
        R["searches"][spec] = got
        log(f"2. search {spec!r} -> {len(got)}")

    # ---- 3. SIMP_SEC_STATUS observed values
    r = ref.createRequest("ReferenceDataRequest")
    for sec in STATUS_BASKET:
        r.getElement("securities").appendValue(sec)
    for f in ("SIMP_SEC_STATUS", "INACTIVE_DATE", "LISTING_DATE", "NAME"):
        r.getElement("fields").appendValue(f)
    s.sendRequest(r)
    status_msgs = drain(blpapi, s)
    R["status_raw"] = status_msgs
    R["status_values"] = field_data(status_msgs, "SIMP_SEC_STATUS")
    log("3. SIMP_SEC_STATUS: " + json.dumps(R["status_values"]))

    # ---- 4. adjustment factors, with and without the override, on AAPL
    R["adjust_factors"] = {}
    for label, overrides in (
        ("default", []),
        # DV175 is EQY_DVD_ADJUST_FACT's only override; resolve it first below.
        ("with_ca_filter", [("CORPORATE_ACTIONS_FILTER", "NORMAL_CASH|ABNORMAL_CASH|CAPITAL_CHANGE")]),
    ):
        r = ref.createRequest("ReferenceDataRequest")
        r.getElement("securities").appendValue("AAPL US Equity")
        r.getElement("fields").appendValue("EQY_DVD_ADJUST_FACT")
        if overrides:
            ov = r.getElement("overrides")
            for fid, val in overrides:
                e = ov.appendElement()
                e.setElement("fieldId", fid)
                e.setElement("value", val)
        s.sendRequest(r)
        msgs = drain(blpapi, s)
        R["adjust_factors"][label] = field_data(msgs, "EQY_DVD_ADJUST_FACT")
        n = len(next(iter(R["adjust_factors"][label].values()), []) or [])
        log(f"4. EQY_DVD_ADJUST_FACT [{label}] -> {n} rows")

    # ---- 5. resolve the override id DV175 to a mnemonic
    ov_ids = sorted({i for v in info.values() for i in (v.get("overrides") or [])})
    if ov_ids:
        r = flds.createRequest("FieldInfoRequest")
        for i in ov_ids:
            r.getElement("id").appendValue(i)
        r.set("returnFieldDocumentation", True)
        s.sendRequest(r)
        R["override_mnemonics"] = field_infos(drain(blpapi, s))
        log("5. overrides resolved: " + ", ".join(sorted(R["override_mnemonics"])))

    s.stop()
    p = "probe_out/p0_close_report.json"
    with open(p, "w", encoding="utf-8") as fh:
        json.dump(R, fh, indent=2, default=str)
    log("wrote " + p)
    return 0


if __name__ == "__main__":
    sys.exit(main())
