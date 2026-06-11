"""Conformance RouteSpecs for the ground-station uplink + modem-usage routes."""

from __future__ import annotations

from ..routes import FieldSpec, Locator, RouteSpec

# The active-uplink snapshot keys the `ados-net` daemon ships on every flag
# change. The aggregate `/network` view reads `active_uplink` back from this
# event instead of the dead in-FastAPI-process router singleton; the other keys
# round out the body a store-first reader sees.
_UPLINK_ACTIVE_FIELDS = [
    "active_uplink",
    "internet_reachable",
    "timestamp_ms",
    "data_cap_state",
]

# The cumulative data-cap usage block the daemon's data-cap tracker ships each
# poll. The `/network/modem` view reads the usage figures back from this event;
# the connectivity legs of that view stay live.
_MODEM_USAGE_FIELDS = [
    "data_used_mb",
    "cap_mb",
    "percent",
    "state",
    "window_reset_at",
    "last_reset_month",
]


def routes() -> list[RouteSpec]:
    """The active-uplink + modem-usage route set (store-backable network legs)."""
    return [
        _uplink_active_route(),
        _modem_usage_route(),
    ]


def _uplink_active_route() -> RouteSpec:
    """Active-uplink snapshot (live events, store-only).

    The aggregate `/network` route reads `active_uplink` back from this event;
    every field is a key in the shipped detail map.
    """
    return RouteSpec(
        name="net-uplink-active",
        kind="events",
        logd_params={
            "kind": "events",
            "event_kind": "net.uplink_active",
            "limit": 200,
        },
        observability_path="/api/v2/observability/events",
        row_match={"kind": "net.uplink_active", "source": "ados-net"},
        fields=[
            FieldSpec(
                field=f,
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-net",
            )
            for f in _UPLINK_ACTIVE_FIELDS
        ],
    )


def _modem_usage_route() -> RouteSpec:
    """Cumulative data-cap usage block (live events, store-only).

    The `/network/modem` route reads the usage figures back from this event;
    the connectivity legs of that view stay live.
    """
    return RouteSpec(
        name="net-modem-usage",
        kind="events",
        logd_params={
            "kind": "events",
            "event_kind": "net.modem_usage",
            "limit": 200,
        },
        observability_path="/api/v2/observability/events",
        row_match={"kind": "net.modem_usage", "source": "ados-net"},
        fields=[
            FieldSpec(
                field=f,
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-net",
            )
            for f in _MODEM_USAGE_FIELDS
        ],
    )
