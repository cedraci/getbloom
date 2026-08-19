#!/usr/bin/env python
"""Bloomberg BLPAPI fetch sidecar for getbloomdata.

Spec: docs/superpowers/specs/2026-08-18-blpapi-redesign.md (Amendment A2).

Replaces the Excel/COM path. Rust spawns this script, writes a request JSON to
stdin, and reads exactly ONE JSON line from stdout (the last line). Diagnostics
go to stderr and are never part of the protocol.

Modes
-----
  --probe                Open a session + //blp/refdata and run one tiny live
                         request. This is the go/no-go gate: it either proves
                         the Desktop API works from a custom app, or it does
                         not, in about ten seconds.
  --replay <raw.json>    Re-parse a previously captured raw response. Needs NO
                         blpapi module and NO Terminal, so all parsing logic is
                         unit-testable anywhere.
  (default)              Read request JSON from stdin, fetch, emit result JSON.

  --raw-out <path>       Also write the raw captured response. This file is
                         both the run's audit trail (spec 4.4) and the exact
                         input format --replay consumes, so today's production
                         payload is tomorrow's test fixture.

Exit codes
----------
  0  ok (including partial results: some securities failed, others returned)
  2  timeout
  3  session / service / transport error, or a structurally empty result
  4  malformed input

Rule inherited from refresh.ps1's worst bug: a failure to start the session,
open the service, or send a request MUST exit non-zero. A result carrying zero
observations AND zero problems is a fault, not a success. Silence is never
success.
"""

import argparse
import datetime as dt
import json
import sys
import time

DEFAULT_HOST = "localhost"
DEFAULT_PORT = 8194
REFDATA_SERVICE = "//blp/refdata"
INSTRUMENTS_SERVICE = "//blp/instruments"

EXIT_OK, EXIT_TIMEOUT, EXIT_SESSION, EXIT_BADINPUT = 0, 2, 3, 4


def log(msg):
    """Diagnostics only. stdout is reserved for the protocol."""
    print(msg, file=sys.stderr, flush=True)


# --------------------------------------------------------------------------
# blpapi element -> plain Python
#
# Kept deliberately separate from parsing so that everything below this point
# is pure data manipulation, testable without blpapi installed.
# --------------------------------------------------------------------------

def _scalar(v):
    if isinstance(v, dt.datetime):
        return v.isoformat()
    if isinstance(v, dt.date):
        return v.isoformat()
    if isinstance(v, dt.time):
        return v.isoformat()
    if isinstance(v, (str, int, float, bool)) or v is None:
        return v
    return str(v)


def elem_to_py(el):
    """Recursively convert a blpapi Element into dicts/lists/scalars.

    Hand-rolled rather than using Message.toPy() so this works across blpapi
    versions, and so the captured shape is stable enough to check in as a
    fixture.
    """
    try:
        if el.isNull():
            return None
    except Exception:
        pass

    if el.isArray():
        out = []
        for i in range(el.numValues()):
            try:
                out.append(elem_to_py(el.getValueAsElement(i)))
            except Exception:
                out.append(_scalar(el.getValue(i)))
        return out

    try:
        n = el.numElements()
    except Exception:
        n = 0
    if n > 0:
        return {str(el.getElement(i).name()): elem_to_py(el.getElement(i))
                for i in range(n)}

    try:
        return _scalar(el.getValue())
    except Exception:
        return None


def message_to_py(msg):
    return elem_to_py(msg.asElement())


# --------------------------------------------------------------------------
# Parsing: raw captured messages -> observations + problems
# --------------------------------------------------------------------------

def as_list(x):
    if x is None:
        return []
    return x if isinstance(x, list) else [x]


def iso_date(value):
    """'20260814' or '2026-08-14' -> '2026-08-14'. None if not a real date.

    Deliberately strict. Bloomberg accepts a nonsense date such as '20261301'
    without complaint and returns an empty result, which is indistinguishable
    from a holiday -- so a bad date would otherwise be laundered into a
    plausible-looking `no_data` issue dated 2026-13-01. Verified live,
    2026-08-18.
    """
    if value is None:
        return None
    s = str(value)
    for text, fmt in ((s[:8], "%Y%m%d"), (s[:10], "%Y-%m-%d")):
        try:
            return dt.datetime.strptime(text, fmt).date().isoformat()
        except ValueError:
            continue
    return None


def validate_request_spec(spec, where="request"):
    """Structural validation. Returns a list of human-readable errors."""
    errors = []
    kind = spec.get("kind")
    if kind not in ("history", "reference", "bulk_reference", "instrument_list"):
        return [f"{where}: unknown request kind: {kind!r}"]

    if kind == "instrument_list":
        if not str(spec.get("query") or "").strip():
            errors.append(f"{where}: instrument_list needs a non-empty query")
        return errors

    if not spec.get("securities"):
        errors.append(f"{where}: no securities")
    if not spec.get("fields"):
        errors.append(f"{where}: no fields")
    for i, ov in enumerate(spec.get("overrides") or []):
        if not ov.get("fieldId") or ov.get("value") is None:
            errors.append(f"{where}: overrides[{i}] needs fieldId and value")
    if kind == "history":
        start, end = iso_date(spec.get("start")), iso_date(spec.get("end"))
        if start is None:
            errors.append(f"{where}: invalid start date {spec.get('start')!r}")
        if end is None:
            errors.append(f"{where}: invalid end date {spec.get('end')!r}")
        if start and end and start > end:
            errors.append(f"{where}: start {start} after end {end}")
    elif kind == "reference" and iso_date(spec.get("obs_date")) is None:
        errors.append(f"{where}: invalid obs_date {spec.get('obs_date')!r}")
    return errors


def validate_payload(payload):
    reqs = payload.get("requests")
    if not isinstance(reqs, list) or not reqs:
        return ["payload has no 'requests'"]
    errors = []
    for i, spec in enumerate(reqs):
        errors.extend(validate_request_spec(spec, f"requests[{i}]"))
    return errors


def classify_security_error(err):
    """Map a securityError element to one of our issue codes."""
    cat = str(err.get("category") or "").upper()
    sub = str(err.get("subcategory") or "").upper()
    if "NOT_ENTITLED" in cat or "NOT_ENTITLED" in sub:
        return "not_entitled"
    if "BAD_SEC" in cat or "INVALID_SECURITY" in sub:
        return "invalid_security"
    return "invalid_security"


def problem(security, field, date, code, detail):
    return {"security": security, "field": field, "date": date,
            "code": code, "detail": detail or ""}


def observation(security, field, date, value):
    """Numbers become `num`; everything else (text, ISO dates) becomes `text`.

    The sidecar deliberately does NOT check against field_def.value_kind — it
    knows nothing about the database. Rust owns the type check (spec 4.5).
    """
    o = {"security": security, "field": field, "date": date}
    if isinstance(value, bool):
        o["text"] = "true" if value else "false"
    elif isinstance(value, (int, float)):
        o["num"] = float(value)
    else:
        o["text"] = str(value)
    return o


def parse_field_exceptions(security, date, sec_data, obs_out, prob_out):
    """fieldExceptions -> field_not_applicable. Returns the set of failed fields."""
    failed = set()
    for fe in as_list(sec_data.get("fieldExceptions")):
        fid = fe.get("fieldId")
        info = fe.get("errorInfo") or {}
        failed.add(fid)
        prob_out.append(problem(
            security, fid, date, "field_not_applicable",
            f"{info.get('category', '')}/{info.get('subcategory', '')}: "
            f"{info.get('message', '')}".strip()))
    return failed


def parse_history_message(req, msg, obs_out, prob_out):
    sec_data = msg.get("securityData") or {}
    # HistoricalDataResponse carries exactly one securityData, unlike reference.
    if isinstance(sec_data, list):
        sec_data = sec_data[0] if sec_data else {}
    security = sec_data.get("security")
    single_day = req.get("start") == req.get("end")
    req_date = iso_date(req.get("start"))
    requested_fields = list(req.get("fields") or [])

    err = sec_data.get("securityError")
    if err:
        code = classify_security_error(err)
        for f in requested_fields:
            prob_out.append(problem(security, f, req_date, code,
                                    err.get("message", "")))
        return

    failed = parse_field_exceptions(security, req_date, sec_data, obs_out, prob_out)

    rows = as_list(sec_data.get("fieldData"))
    if not rows:
        # No trading day in range. For a single-day EOD run this is the holiday
        # case and must surface per field (spec 4.5, preserving Amendment A1).
        # For a multi-day backfill we cannot know which days should have
        # existed, so absence is simply a gap -- consistent with the
        # no-holiday-calendar design.
        if single_day:
            for f in requested_fields:
                if f not in failed:
                    prob_out.append(problem(security, f, req_date, "no_data",
                                            "no trading day returned"))
        return

    for row in rows:
        d = iso_date(row.get("date"))
        for f in requested_fields:
            if f in failed:
                continue
            if f in row:
                obs_out.append(observation(security, f, d, row[f]))
            elif single_day:
                prob_out.append(problem(security, f, d, "no_data",
                                        "field absent from returned row"))


def parse_reference_message(req, msg, obs_out, prob_out):
    obs_date = iso_date(req.get("obs_date"))
    requested_fields = list(req.get("fields") or [])

    for sec_data in as_list(msg.get("securityData")):
        security = sec_data.get("security")

        err = sec_data.get("securityError")
        if err:
            code = classify_security_error(err)
            for f in requested_fields:
                prob_out.append(problem(security, f, obs_date, code,
                                        err.get("message", "")))
            continue

        failed = parse_field_exceptions(security, obs_date, sec_data,
                                        obs_out, prob_out)
        fdata = sec_data.get("fieldData") or {}
        for f in requested_fields:
            if f in failed:
                continue
            if f in fdata and fdata[f] is not None:
                obs_out.append(observation(security, f, obs_date, fdata[f]))
            else:
                prob_out.append(problem(security, f, obs_date, "no_data",
                                        "field absent from response"))


def parse_bulk_message(spec, msg, rows_out, problems_out):
    """Bulk (table-valued) reference fields.

    P0 5: a field whose ftype is 'BulkFormat' returns a LIST OF DICTS, not a
    scalar. Reading it with the scalar path would coerce a whole corporate-action
    table into one meaningless string, so bulk fields get their own kind and
    their own output section.
    """
    for sec_data in msg.get("securityData", []):
        security = sec_data.get("security")
        sec_err = sec_data.get("securityError")
        if sec_err:
            problems_out.append(problem(
                security, None, None, classify_security_error(sec_err),
                sec_err.get("message", "")))
            continue
        failed = {}
        for exc in sec_data.get("fieldExceptions", []):
            info = exc.get("errorInfo") or {}
            failed[exc.get("fieldId")] = info
            problems_out.append(problem(
                security, exc.get("fieldId"), None, "field_error",
                info.get("message", "")))
        fdata = sec_data.get("fieldData") or {}
        for f in spec.get("fields", []):
            if f in failed:
                continue
            value = fdata.get(f)
            if not value:
                problems_out.append(problem(
                    security, f, None, "no_data", "bulk field absent or empty"))
                continue
            # toPy() gives a list of dicts for a bulk field. A scalar here means
            # the field is not actually bulk, which is worth saying out loud.
            if not isinstance(value, list):
                problems_out.append(problem(
                    security, f, None, "not_bulk",
                    f"expected a table, got {type(value).__name__}"))
                continue
            rows_out.append({"security": security, "field": f,
                             "rows": [dict(r) for r in value]})


def parse_instrument_list_message(msg, out):
    """instrumentListRequest results, kept exactly as returned.

    The 'AAPL US<equity>' form is NOT normalised here: Rust owns that rule and
    its regression test, and a sidecar that silently rewrote identifiers would
    put the conversion beyond the reach of those tests.
    """
    for r in msg.get("results", []):
        out.append({"security": r.get("security"),
                    "description": r.get("description", "")})


def parse_capture(capture):
    """capture -> (observations, problems, bulk_rows, list_results, fatal|None).

    `capture` is {"run_id":.., "captured":[{"request":{..},"messages":[..]}]}
    -- exactly what --raw-out writes and --replay reads.
    """
    observations, problems, bulk_rows, list_results = [], [], [], []
    for item in capture.get("captured", []):
        req = item.get("request") or {}
        kind = req.get("kind")
        # Guard replay too: a malformed capture must fail loudly rather than
        # produce observations stamped with impossible dates.
        spec_errors = validate_request_spec(req)
        if spec_errors:
            return observations, problems, bulk_rows, list_results, "; ".join(spec_errors)
        for msg in item.get("messages", []):
            resp_err = msg.get("responseError")
            if resp_err:
                # Request-level failure: not attributable to any one security,
                # so it is fatal rather than a per-cell problem.
                return observations, problems, bulk_rows, list_results, (
                    f"responseError {resp_err.get('category', '')}: "
                    f"{resp_err.get('message', '')}")
            if kind == "history":
                parse_history_message(req, msg, observations, problems)
            elif kind == "reference":
                parse_reference_message(req, msg, observations, problems)
            elif kind == "bulk_reference":
                parse_bulk_message(req, msg, bulk_rows, problems)
            else:
                parse_instrument_list_message(msg, list_results)
    return observations, problems, bulk_rows, list_results, None


# --------------------------------------------------------------------------
# BLPAPI session
# --------------------------------------------------------------------------

def import_blpapi():
    try:
        import blpapi  # noqa: F401
        return blpapi
    except ImportError as e:
        raise SessionError(
            "the 'blpapi' module is not installed. Install it from Bloomberg's "
            "package index: pip install --index-url="
            "https://blpapi.bloomberg.com/repository/releases/python/simple/ blpapi"
        ) from e


class SessionError(Exception):
    pass


class TimeoutError_(Exception):
    pass


def open_session(blpapi, host, port):
    opts = blpapi.SessionOptions()
    opts.setServerHost(host)
    opts.setServerPort(port)
    session = blpapi.Session(opts)
    if not session.start():
        raise SessionError(
            f"session.start() failed against {host}:{port} -- is the Bloomberg "
            "Terminal running and logged in as this Windows user?")
    if not session.openService(REFDATA_SERVICE):
        session.stop()
        raise SessionError(
            f"openService('{REFDATA_SERVICE}') failed -- the session connected "
            "but the refdata service was refused (entitlement?)")
    # Optional: only instrument_list needs it, and a run that never searches
    # should not fail because the search service is unavailable.
    session.openService(INSTRUMENTS_SERVICE)
    return session


def build_request(blpapi, service, spec):
    kind = spec.get("kind")
    if kind == "history":
        r = service.createRequest("HistoricalDataRequest")
        r.set("startDate", spec["start"])
        r.set("endDate", spec["end"])
        r.set("periodicitySelection", "DAILY")
        # Holidays must come back as *absent* rows, not as filled-forward
        # values -- that is what makes the `no_data` issue honest.
        r.set("nonTradingDayFillOption", "ACTIVE_DAYS_ONLY")
    elif kind in ("reference", "bulk_reference"):
        r = service.createRequest("ReferenceDataRequest")
    elif kind == "instrument_list":
        r = service.createRequest("instrumentListRequest")
        r.set("query", spec["query"])
        if spec.get("yellow_key_filter"):
            r.set("yellowKeyFilter", spec["yellow_key_filter"])
        r.set("maxResults", int(spec.get("max_results", 20)))
        return r
    else:
        raise SessionError(f"unknown request kind: {kind!r}")

    for s in spec.get("securities", []):
        r.getElement("securities").appendValue(s)
    for f in spec.get("fields", []):
        r.getElement("fields").appendValue(f)

    # Field overrides. HISTORICAL_IDS_TIME_RANGE needs
    # HISTORICAL_STARTING_IDENTIFIER; without it Bloomberg answers about a
    # different instrument that once wore the same ticker (P0 6.4).
    overrides = spec.get("overrides") or []
    if overrides:
        ov = r.getElement("overrides")
        for o in overrides:
            e = ov.appendElement()
            e.setElement("fieldId", o["fieldId"])
            e.setElement("value", str(o["value"]))
    return r


def send_and_drain(blpapi, session, request, deadline):
    """Send one request, accumulate PARTIAL_RESPONSE, stop at RESPONSE."""
    session.sendRequest(request)
    messages = []
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError_("deadline exceeded while awaiting response")
        ev = session.nextEvent(int(min(remaining, 5.0) * 1000))
        et = ev.eventType()
        if et in (blpapi.Event.PARTIAL_RESPONSE, blpapi.Event.RESPONSE):
            for msg in ev:
                messages.append(message_to_py(msg))
            if et == blpapi.Event.RESPONSE:
                return messages
        elif et == blpapi.Event.SESSION_STATUS:
            for msg in ev:
                name = str(msg.messageType())
                if name in ("SessionTerminated", "SessionStartupFailure"):
                    raise SessionError(f"session status: {name}")
        # TIMEOUT events are normal: they just mean "nothing yet", keep waiting.


def run_fetch(payload, raw_out):
    blpapi = import_blpapi()
    timeout_s = float(payload.get("timeout_s") or 120)
    deadline = time.monotonic() + timeout_s

    session = open_session(blpapi,
                           payload.get("host", DEFAULT_HOST),
                           int(payload.get("port", DEFAULT_PORT)))
    captured = []
    try:
        refdata_service = session.getService(REFDATA_SERVICE)
        for spec in payload.get("requests", []):
            # instrument_list lives on //blp/instruments, not //blp/refdata --
            # a request built against the wrong service object fails at the
            # blpapi layer with an opaque error, so route it explicitly.
            if spec.get("kind") == "instrument_list":
                service = session.getService(INSTRUMENTS_SERVICE)
            else:
                service = refdata_service
            req = build_request(blpapi, service, spec)
            log(f"  -> {spec.get('kind')}: {len(spec.get('securities', []))} "
                f"securities x {len(spec.get('fields', []))} fields")
            messages = send_and_drain(blpapi, session, req, deadline)
            captured.append({"request": spec, "messages": messages})
    finally:
        try:
            session.stop()
        except Exception:
            pass

    capture = {"run_id": payload.get("run_id"), "captured": captured}
    if raw_out:
        with open(raw_out, "w", encoding="utf-8") as fh:
            json.dump(capture, fh, indent=1)
        log(f"raw response written to {raw_out}")
    return capture


# --------------------------------------------------------------------------
# Output protocol
# --------------------------------------------------------------------------

def emit(status, seconds, detail, observations, problems, code,
        bulk_rows=None, list_results=None, raw_messages=None):
    print(json.dumps({
        "status": status,
        "seconds": round(seconds, 2),
        "detail": detail or "",
        "observations": observations,
        "problems": problems,
        "bulk_rows": bulk_rows or [],
        "list_results": list_results or [],
        "raw_messages": raw_messages or [],
    }, separators=(",", ":")))
    sys.stdout.flush()
    return code


def collect_raw_messages(capture):
    """Flat messages for requests that asked for them.

    A FLAT array, not the {"request":.., "messages":[..]} items --raw-out
    writes: the master-fetch parsers on the Rust side walk securityData
    directly, and the committed P0 capture files are already in this shape.
    Gated on the request's own "raw" flag so an EOD history run, which never
    asks for it, does not pay for messages nothing will read.
    """
    raw_messages = []
    for item in capture.get("captured", []):
        if (item.get("request") or {}).get("raw"):
            raw_messages.extend(item.get("messages", []))
    return raw_messages


def finish(capture, started):
    observations, problems, bulk_rows, list_results, fatal = parse_capture(capture)
    seconds = time.monotonic() - started
    if fatal:
        return emit("error", seconds, fatal, [], [], EXIT_SESSION)
    raw_messages = collect_raw_messages(capture)
    if not observations and not problems and not bulk_rows and not list_results \
            and not raw_messages:
        # Structurally impossible for a well-formed request. Treated as a
        # fault so a silent no-op can never look like a successful run.
        return emit("empty", seconds,
                    "response contained neither observations nor problems",
                    [], [], EXIT_SESSION)
    log(f"{len(observations)} observations, {len(problems)} problems, "
        f"{len(bulk_rows)} bulk rows, {len(list_results)} list results")
    return emit("ok", seconds, "", observations, problems, EXIT_OK,
               bulk_rows, list_results, raw_messages)


# --------------------------------------------------------------------------
# Modes
# --------------------------------------------------------------------------

def do_probe(args):
    started = time.monotonic()
    print(f"BLPAPI probe -> {args.host}:{args.port}")
    try:
        blpapi = import_blpapi()
    except SessionError as e:
        print(f"  [FAIL] {e}")
        return EXIT_SESSION
    print(f"  [ok]   blpapi module imported (version "
          f"{getattr(blpapi, '__version__', 'unknown')})")

    try:
        session = open_session(blpapi, args.host, args.port)
    except SessionError as e:
        print(f"  [FAIL] {e}")
        return EXIT_SESSION
    print(f"  [ok]   session started and {REFDATA_SERVICE} opened")

    try:
        service = session.getService(REFDATA_SERVICE)
        spec = {"kind": "reference", "securities": [args.security],
                "fields": ["NAME", "PX_LAST"],
                "obs_date": dt.date.today().isoformat()}
        req = build_request(blpapi, service, spec)
        messages = send_and_drain(blpapi, session, req,
                                  time.monotonic() + args.timeout)
    except (SessionError, TimeoutError_) as e:
        print(f"  [FAIL] live request failed: {e}")
        return EXIT_SESSION
    finally:
        try:
            session.stop()
        except Exception:
            pass

    obs, probs, _bulk_rows, _list_results, fatal = parse_capture(
        {"captured": [{"request": spec, "messages": messages}]})
    if fatal:
        print(f"  [FAIL] {fatal}")
        return EXIT_SESSION
    for o in obs:
        print(f"  [ok]   {o['security']} {o['field']} = "
              f"{o.get('num', o.get('text'))}")
    for p in probs:
        print(f"  [warn] {p['security']} {p['field']}: {p['code']} {p['detail']}")
    if not obs:
        print("  [FAIL] connected, but no data came back for "
              f"{args.security} -- check the ticker and your entitlements")
        return EXIT_SESSION
    print(f"\n  GREEN LIGHT: Desktop API works from a custom app "
          f"({time.monotonic() - started:.1f}s).")
    return EXIT_OK


def main():
    ap = argparse.ArgumentParser(description="Bloomberg BLPAPI fetch sidecar")
    ap.add_argument("--probe", action="store_true",
                    help="connect, open refdata, run one live request, report")
    ap.add_argument("--replay", metavar="FILE",
                    help="re-parse a captured raw response (no blpapi needed)")
    ap.add_argument("--raw-out", metavar="FILE",
                    help="write the raw captured response (audit trail + fixture)")
    ap.add_argument("--security", default="IBM US Equity",
                    help="security to use for --probe")
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--timeout", type=float, default=60.0,
                    help="probe timeout in seconds")
    args = ap.parse_args()

    if args.probe:
        return do_probe(args)

    started = time.monotonic()

    if args.replay:
        try:
            with open(args.replay, encoding="utf-8") as fh:
                capture = json.load(fh)
        except (OSError, ValueError) as e:
            return emit("bad-input", time.monotonic() - started,
                        f"cannot read replay file: {e}", [], [], EXIT_BADINPUT)
        return finish(capture, started)

    try:
        payload = json.load(sys.stdin)
    except ValueError as e:
        return emit("bad-input", time.monotonic() - started,
                    f"stdin is not valid JSON: {e}", [], [], EXIT_BADINPUT)
    errors = validate_payload(payload)
    if errors:
        return emit("bad-input", time.monotonic() - started,
                    "; ".join(errors), [], [], EXIT_BADINPUT)

    try:
        capture = run_fetch(payload, args.raw_out)
    except TimeoutError_ as e:
        return emit("timeout", time.monotonic() - started, str(e), [], [],
                    EXIT_TIMEOUT)
    except SessionError as e:
        return emit("session-error", time.monotonic() - started, str(e), [], [],
                    EXIT_SESSION)
    except Exception as e:  # noqa: BLE001 - any escape must still exit non-zero
        return emit("error", time.monotonic() - started,
                    f"{type(e).__name__}: {e}", [], [], EXIT_SESSION)

    return finish(capture, started)


if __name__ == "__main__":
    sys.exit(main())
