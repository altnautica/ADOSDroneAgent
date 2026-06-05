"""Conformance harness for the durable logging-and-telemetry store.

A standalone, deterministic, bounded dual-run checker. For each route in the
first observability set it queries the legacy on-box log/telemetry handlers and
the durable store's query surface, then asserts the store serves a superset of
the fields the legacy handler exposes, classifying every field as durable
history or a live read. It emits a machine-readable JSON report so a coding
agent can see, per route and per field, whether the store is a superset, where a
field is missing, and where a producer is not yet emitting.

The package is import-safe with no side effects; the CLI lives in the sibling
``main.py``.
"""

from .routes import FieldSpec, Locator, RouteSpec, initial_routes
from .runner import (
    FieldResult,
    Report,
    RouteResult,
    field_status,
    run_conformance,
)

__all__ = [
    "FieldSpec",
    "Locator",
    "RouteSpec",
    "initial_routes",
    "FieldResult",
    "RouteResult",
    "Report",
    "run_conformance",
    "field_status",
]
