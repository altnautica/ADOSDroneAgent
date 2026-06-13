"""Conformance harness for the native control front versus the residual Python.

A standalone, deterministic, bounded dual-run checker. For each route it issues
the identical HTTP request to two transports — the native control front (over
TCP) and the residual Python handler (over its internal unix socket) — and
asserts the two responses are byte-faithfully equal, modulo a small set of
volatile fields (timestamps, pids, monotonic counters). This is the gate that
lets a route flip from proxied-to-Python to native-in-Rust: a ported native
handler must match Python before the cutover.

It is a developer tool. It is **not** part of the agent runtime and is not run
during install.

The package is import-safe with no side effects; the CLI lives in the sibling
``main.py``.
"""

from .normalize import (
    canonical_json,
    compare_headers,
    normalize_body,
    normalize_sse_frames,
)
from .route_cases import REGISTRY, RouteCase, case_by_name, registered_cases
from .runner import (
    CaseResult,
    Report,
    assert_response_equal,
    run_conformance,
)

__all__ = [
    "RouteCase",
    "REGISTRY",
    "registered_cases",
    "case_by_name",
    "CaseResult",
    "Report",
    "assert_response_equal",
    "run_conformance",
    "canonical_json",
    "compare_headers",
    "normalize_body",
    "normalize_sse_frames",
]
