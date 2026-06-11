"""Conformance spec for the video-encoder metric route."""

from __future__ import annotations

from ..routes import FieldSpec, Locator, RouteSpec


def routes() -> list[RouteSpec]:
    """The video-encoder route set."""
    return [_video_metrics_route()]


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
