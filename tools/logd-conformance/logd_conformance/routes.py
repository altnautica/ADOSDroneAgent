"""The first observability set: the routes and fields the harness checks.

Each ``RouteSpec`` names a durable-store table (``logs`` / ``events`` /
``metrics`` / ``hw``), the legacy on-box handler it should eventually subsume
(when one exists), and the set of fields the store must serve. Every
``FieldSpec`` records where the field lives in a store row, whether it is durable
history or a live read, and which producer emits it, so the report can tell a
genuine schema gap (the producer emits rows but not this field) apart from a
producer that is simply not running (no rows at all).

The legacy field map for the log route mirrors the on-box legacy entry mapping
one-for-one: ``seq←id``, ``timestamp←ts_us``, ``level←level``,
``logger←target|source``, ``message←msg``.
"""

from __future__ import annotations

from enum import Enum

from pydantic import BaseModel, Field


class Locator(str, Enum):
    """Where a field lives inside a durable-store query row.

    * ``row_key`` — a top-level column on a ``logs`` / ``events`` row.
    * ``metric`` — the dotted name carried in a ``metrics`` row's ``metric``
      column (each metric is its own row, so absence means the producer is not
      emitting, never a schema gap).
    * ``detail_key`` — a key inside an ``events`` row's open ``detail`` map.
    * ``signal`` — a key inside an ``hw`` row's open ``signals`` map.
    """

    ROW_KEY = "row_key"
    METRIC = "metric"
    DETAIL_KEY = "detail_key"
    SIGNAL = "signal"


class FieldSpec(BaseModel):
    """One field the store must serve for a route."""

    field: str
    locator: Locator
    classification: str  # "history" | "live"
    producer: str
    # The corresponding legacy field, when the route has a legacy handler to be
    # a superset of. ``None`` for store-only routes (telemetry / events that the
    # legacy surface never served).
    legacy_field: str | None = None


class RouteSpec(BaseModel):
    """A route under conformance: a store table plus the fields it must serve."""

    name: str
    kind: str  # "logs" | "events" | "metrics" | "hw"
    # Query params for the store's /v1/query endpoint.
    logd_params: dict[str, object] = Field(default_factory=dict)
    # The legacy on-box handler path, when one exists to compare against.
    legacy_path: str | None = None
    # Key under which the legacy response carries its row list.
    legacy_entries_key: str = "entries"
    # The /api/v2/observability proxy path, when wired (Stage 2 wires it).
    observability_path: str | None = None
    # An equality filter applied to store rows before the field check (e.g. an
    # events route restricting to one event ``kind``). Empty means all rows of
    # the kind count.
    row_match: dict[str, str] = Field(default_factory=dict)
    fields: list[FieldSpec]


def initial_routes() -> list[RouteSpec]:
    """The full registered route set, in report order."""
    from .route_specs import all_route_specs

    return all_route_specs()


def route_by_name(name: str) -> RouteSpec | None:
    """Look up one route by name (for the ``--route`` CLI filter)."""
    for route in initial_routes():
        if route.name == name:
            return route
    return None
