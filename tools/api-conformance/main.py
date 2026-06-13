#!/usr/bin/env python3
"""CLI for the native-vs-Python API conformance harness.

Issues the identical HTTP request to the native control front (over TCP) and the
residual Python handler (over its internal unix socket) and asserts the two
responses are byte-faithfully equal, modulo volatile fields, per route. This is
the gate a route clears before it flips from proxied-to-Python to native-in-Rust.
It prints a JSON report to stdout and exits non-zero when any diffed route failed
(or, under ``--strict``, when any sandboxed route was skipped).

Examples::

    python tools/api-conformance/main.py \
        --front-base http://localhost:8080 \
        --python-uds /run/ados/api-internal.sock

    python tools/api-conformance/main.py --route status --strict
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# Allow ``python tools/api-conformance/main.py`` to import the sibling package
# without an install step (the script dir is on sys.path, but be explicit).
sys.path.insert(0, str(Path(__file__).resolve().parent))

from api_conformance.client import DEFAULT_TIMEOUT_S, Clients  # noqa: E402
from api_conformance.route_cases import case_by_name, registered_cases  # noqa: E402
from api_conformance.runner import run_conformance  # noqa: E402


def _parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="api-conformance",
        description="Assert the native control front and the residual Python "
        "return byte-faithfully equal responses, per route.",
    )
    p.add_argument(
        "--front-base",
        default="http://localhost:8080",
        help="Base URL of the native control front over TCP "
        "(default: http://localhost:8080).",
    )
    p.add_argument(
        "--python-uds",
        default="/run/ados/api-internal.sock",
        help="Path to the residual Python's internal unix socket "
        "(default: /run/ados/api-internal.sock).",
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
        help="Also fail when any sandboxed route was skipped, not just on a "
        "diff failure.",
    )
    p.add_argument(
        "--json",
        action="store_true",
        help="Emit the JSON report (the default output is already JSON; this "
        "flag is accepted for symmetry and future plain-text modes).",
    )
    return p.parse_args(argv)


def _select_cases(names: list[str] | None):
    if not names:
        return registered_cases()
    selected = []
    for name in names:
        case = case_by_name(name)
        if case is None:
            known = ", ".join(c.name for c in registered_cases())
            raise SystemExit(f"unknown route '{name}'; known routes: {known}")
        selected.append(case)
    return selected


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(sys.argv[1:] if argv is None else argv)
    cases = _select_cases(args.route)

    with Clients.connect(
        front_base=args.front_base,
        python_uds=args.python_uds,
        timeout=args.timeout,
    ) as clients:
        report = run_conformance(clients, cases, strict=args.strict)

    print(json.dumps(report.model_dump(), indent=2))
    return 0 if report.ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
