"""Conformance spec for the video metric + latency routes."""

from __future__ import annotations

from ..routes import FieldSpec, Locator, RouteSpec


def routes() -> list[RouteSpec]:
    """The video route set: encoder metrics + latency."""
    return [
        _video_metrics_route(),
        _video_latency_metrics_route(),
    ]


def _video_metrics_route() -> RouteSpec:
    """Air-side video encoder telemetry (live telemetry, store-only).

    `queue_depth_frames` / `dropped_frames_cumulative` are intentionally absent:
    the streaming-copy path has no live source for them, so the producer no
    longer emits a placeholder and the conformance surface does not require one.
    """
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
        ],
    )


def _video_latency_metrics_route() -> RouteSpec:
    """SEI glass-to-glass latency fields the latency route reads back."""
    names = [
        "video.latency.glass_ms",
        "video.latency.ewma_ms",
        "video.latency.samples",
    ]
    return RouteSpec(
        name="video-latency",
        kind="metrics",
        logd_params={"kind": "metrics", "limit": 200},
        observability_path="/api/v2/observability/metrics",
        fields=[
            FieldSpec(
                field=name,
                locator=Locator.METRIC,
                classification="live",
                producer="ados-logd",
            )
            for name in names
        ],
    )
