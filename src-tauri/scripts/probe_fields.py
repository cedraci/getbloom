#!/usr/bin/env python
"""Field-dictionary search via //blp/apiflds (FieldSearchRequest).

Companion diagnostic to probe_merger.py: instead of guessing mnemonics and
paying a hit to learn each one is invalid, ask Bloomberg's own field
dictionary what exists. Metadata requests on //blp/apiflds are not data
hits. Prints mnemonic, datatype/ftype and description for every match.

    python probe_fields.py merger
    python probe_fields.py "target shares" --max 80
"""

import argparse
import sys
import time

import blp_fetch as bf

APIFLDS_SERVICE = "//blp/apiflds"


def main():
    ap = argparse.ArgumentParser(description="BLPAPI field dictionary search")
    ap.add_argument("spec", help="search text, e.g. 'merger'")
    ap.add_argument("--max", type=int, default=60, help="max results printed")
    ap.add_argument("--host", default=bf.DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=bf.DEFAULT_PORT)
    ap.add_argument("--timeout", type=float, default=60.0)
    args = ap.parse_args()

    blpapi = bf.import_blpapi()
    session = bf.open_session(blpapi, args.host, args.port)
    try:
        if not session.openService(APIFLDS_SERVICE):
            print(f"[FAIL] openService('{APIFLDS_SERVICE}') refused")
            return bf.EXIT_SESSION
        service = session.getService(APIFLDS_SERVICE)
        req = service.createRequest("FieldSearchRequest")
        req.set("searchSpec", args.spec)
        messages = bf.send_and_drain(blpapi, session, req,
                                     time.monotonic() + args.timeout)
    finally:
        try:
            session.stop()
        except Exception:
            pass

    shown = 0
    for msg in messages:
        for fd in bf.as_list(msg.get("fieldData")):
            info = fd.get("fieldInfo") or {}
            if not info:
                # fieldError entries carry the id but no info; skip quietly.
                continue
            if shown >= args.max:
                print(f"... more results not shown (--max {args.max})")
                return bf.EXIT_OK
            shown += 1
            print(f"{info.get('mnemonic', '?'):34} "
                  f"{info.get('datatype', '?'):10} "
                  f"{info.get('ftype', ''):12} "
                  f"{info.get('description', '')}")
    print(f"\n{shown} field(s) matched {args.spec!r}")
    return bf.EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
