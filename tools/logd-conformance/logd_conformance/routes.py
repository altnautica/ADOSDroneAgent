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


def _log_route() -> RouteSpec:
    """The durable log route, the canonical dual-run against the legacy handler.

    The legacy entry shape is ``{ seq, timestamp, level, logger, message }``;
    the store row is ``{ id, ts_us, source, level, target, msg, ... }``. Every
    legacy field maps onto a store column the store already serves, so the store
    is a strict superset (it additionally carries ``session``, ``fields`` and the
    redaction flag).
    """
    return RouteSpec(
        name="logs",
        kind="logs",
        logd_params={"kind": "logs", "limit": 50},
        legacy_path="/api/logs",
        legacy_entries_key="entries",
        observability_path="/api/v2/observability/logs",
        fields=[
            FieldSpec(
                field="id",
                locator=Locator.ROW_KEY,
                classification="history",
                producer="any",
                legacy_field="seq",
            ),
            FieldSpec(
                field="ts_us",
                locator=Locator.ROW_KEY,
                classification="history",
                producer="any",
                legacy_field="timestamp",
            ),
            FieldSpec(
                field="level",
                locator=Locator.ROW_KEY,
                classification="history",
                producer="any",
                legacy_field="level",
            ),
            FieldSpec(
                field="source",
                locator=Locator.ROW_KEY,
                classification="history",
                producer="any",
                legacy_field="logger",
            ),
            FieldSpec(
                field="msg",
                locator=Locator.ROW_KEY,
                classification="history",
                producer="any",
                legacy_field="message",
            ),
        ],
    )


def _link_metrics_route() -> RouteSpec:
    """Radio / ground link quality samples (live telemetry, store-only)."""
    return RouteSpec(
        name="link-metrics",
        kind="metrics",
        logd_params={"kind": "metrics", "limit": 200},
        observability_path="/api/v2/observability/metrics",
        fields=[
            FieldSpec(
                field="link.rssi_dbm",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-radio|ados-groundlink",
            ),
            FieldSpec(
                field="link.snr_db",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-radio|ados-groundlink",
            ),
            FieldSpec(
                field="link.fec_uncorrected",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-radio|ados-groundlink",
            ),
        ],
    )


def _video_metrics_route() -> RouteSpec:
    """Air-side video encoder telemetry (live telemetry, store-only)."""
    return RouteSpec(
        name="video-metrics",
        kind="metrics",
        logd_params={"kind": "metrics", "limit": 200},
        observability_path="/api/v2/observability/metrics",
        fields=[
            FieldSpec(
                field="video.encoder_bitrate_kbps",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-video",
            ),
            FieldSpec(
                field="video.framerate_hz",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-video",
            ),
            FieldSpec(
                field="video.queue_depth_frames",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-video",
            ),
            FieldSpec(
                field="video.dropped_frames_cumulative",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-video",
            ),
        ],
    )


def _hw_summary_route() -> RouteSpec:
    """Headline hardware summary metrics (live telemetry, store-only)."""
    return RouteSpec(
        name="hw-summary",
        kind="metrics",
        logd_params={"kind": "metrics", "limit": 200},
        observability_path="/api/v2/observability/metrics",
        fields=[
            FieldSpec(
                field="cpu.utilization_pct",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-logd",
            ),
            FieldSpec(
                field="mem.available_pct",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-logd",
            ),
            FieldSpec(
                field="disk.used_pct",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-logd",
            ),
            FieldSpec(
                field="thermal.primary_c",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-logd",
            ),
        ],
    )


def _hw_snapshot_route() -> RouteSpec:
    """Hardware-snapshot signals the resource routes read back (store-only).

    The diagnostics/status/system routes derive their CPU/memory/disk/temperature
    /load readouts from the most-recent hw snapshots, so the store must serve each
    backing signal in the ``hw`` row's open ``signals`` map. These are the
    net-new fields the collector grew to fully cover those routes (memory cache,
    swap total, filesystem bytes, load average) plus the spine it already carried.
    """
    names = [
        "mem.total_bytes",
        "mem.avail_bytes",
        "mem.cache_bytes",
        "mem.swap_total_bytes",
        "disk.fs_total_bytes",
        "disk.fs_used_bytes",
        "sched.loadavg_1",
        "thermal.primary_c",
        "cpu.util.all",
    ]
    return RouteSpec(
        name="hw-snapshot",
        kind="hw",
        logd_params={"kind": "hw", "limit": 200},
        observability_path="/api/v2/observability/hw",
        fields=[
            FieldSpec(
                field=name,
                locator=Locator.SIGNAL,
                classification="live",
                producer="ados-logd",
            )
            for name in names
        ],
    )


def _service_events_route() -> RouteSpec:
    """Supervisor service-transition events (live events, store-only)."""
    return RouteSpec(
        name="service-events",
        kind="events",
        logd_params={"kind": "events", "limit": 200},
        observability_path="/api/v2/observability/events",
        row_match={"kind": "service.transition"},
        fields=[
            FieldSpec(
                field="from_state",
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-supervisor",
            ),
            FieldSpec(
                field="to_state",
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-supervisor",
            ),
            FieldSpec(
                field="reason",
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-supervisor",
            ),
        ],
    )


def initial_routes() -> list[RouteSpec]:
    """The first set of routes the harness checks, in report order."""
    return [
        _log_route(),
        _link_metrics_route(),
        _video_metrics_route(),
        _hw_summary_route(),
        _hw_snapshot_route(),
        _service_events_route(),
    ]


def route_by_name(name: str) -> RouteSpec | None:
    """Look up one route by name (for the ``--route`` CLI filter)."""
    for route in initial_routes():
        if route.name == name:
            return route
    return None
