"""Conformance spec for the link-quality metric route."""

from __future__ import annotations

from ..routes import FieldSpec, Locator, RouteSpec


def routes() -> list[RouteSpec]:
    """The link-quality route set."""
    return [_link_metrics_route()]


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
