#!/usr/bin/env python3
"""CLI for the durable-store conformance harness.

Runs the first observability set against a live agent: it queries the legacy
on-box handlers and the durable store (the on-box unix socket first, the LAN TCP
port as the fallback) and asserts the store serves a superset of the legacy
fields, classifying each as durable history or a live read. It prints a JSON
report to stdout and exits non-zero when any field failed (or, under
``--strict``, when any producer is missing).

Examples::

    python tools/logd-conformance/main.py \
        --legacy-base http://localhost:8080 \
        --logd-base http://localhost:8090 \
        --socket /run/ados/logd-query.sock

    python tools/logd-conformance/main.py --route hw-summary --strict
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# Allow ``python tools/logd-conformance/main.py`` to import the sibling package
# without an install step (the script dir is on sys.path, but be explicit).
sys.path.insert(0, str(Path(__file__).resolve().parent))

from logd_conformance.client import DEFAULT_TIMEOUT_S, Fetcher  # noqa: E402
from logd_conformance.routes import initial_routes, route_by_name  # noqa: E402
from logd_conformance.runner import run_conformance  # noqa: E402


def _parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="logd-conformance",
        description="Assert the durable store is a superset of the legacy "
        "log/telemetry surface, per route and per field.",
    )
    p.add_argument(
        "--legacy-base",
        default="http://localhost:8080",
        help="Base URL of the legacy on-box handlers and the observability "
        "proxy (default: http://localhost:8080).",
    )
    p.add_argument(
        "--logd-base",
        default="http://localhost:8090",
        help="Base URL of the store's LAN query port, the TCP fallback "
        "(default: http://localhost:8090).",
    )
    p.add_argument(
        "--socket",
        default="/run/ados/logd-query.sock",
        help="Path to the store's on-box query unix socket, tried before the "
        "TCP base (default: /run/ados/logd-query.sock).",
    )
    p.add_argument(
        "--route",
        action="append",
        default=None,
        help="Restrict to one route by name (repeatable). Default: all.",
    )
    p.add_argument(
        "--timeout",
        type=float,
        default=DEFAULT_TIMEOUT_S,
        help=f"Per-request timeout in seconds (default: {DEFAULT_TIMEOUT_S}).",
    )
    p.add_argument(
        "--strict",
        action="store_true",
        help="Also fail when any producer is missing (no rows), not just on a "
        "field gap.",
    )
    p.add_argument(
        "--no-socket",
        action="store_true",
        help="Skip the unix socket and use only the TCP base.",
    )
    return p.parse_args(argv)


def _select_routes(names: list[str] | None):
    if not names:
        return initial_routes()
    selected = []
    for name in names:
        route = route_by_name(name)
        if route is None:
            known = ", ".join(r.name for r in initial_routes())
            raise SystemExit(f"unknown route '{name}'; known routes: {known}")
        selected.append(route)
    return selected


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(sys.argv[1:] if argv is None else argv)
    routes = _select_routes(args.route)
    socket = None if args.no_socket else args.socket

    with Fetcher.connect(
        legacy_base=args.legacy_base,
        logd_base=args.logd_base,
        socket=socket,
        timeout=args.timeout,
    ) as fetcher:
        report = run_conformance(fetcher, routes, strict=args.strict)

    print(json.dumps(report.model_dump(), indent=2))
    return 0 if report.ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
