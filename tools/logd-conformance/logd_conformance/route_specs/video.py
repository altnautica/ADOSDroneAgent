"""Conformance spec for the video metric + air-pipeline + latency routes."""

from __future__ import annotations

from ..routes import FieldSpec, Locator, RouteSpec


def routes() -> list[RouteSpec]:
    """The video route set: encoder metrics + air-pipeline + latency."""
    return [
        _video_metrics_route(),
        _air_pipeline_metrics_route(),
        _air_pipeline_state_event_route(),
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


def _air_pipeline_metrics_route() -> RouteSpec:
    """Air-pipeline numeric fields the air-pipeline route reads back.

    The sidecar tailer samples every numeric / bool field of the air-pipeline
    snapshot into this series so the route reconstructs the body from the store.
    The three monotonic-clock floats are not here: they carry no cross-process
    meaning, so the route serves them live.
    """
    names = [
        "video.air.encoder_fps",
        "video.air.encoded_kbps",
        "video.air.sei_injected_count",
        "video.air.udp_bytes_out",
        "video.air.restart_count",
        "video.air.tx_silent_kicks",
        "video.air.bus_errors",
        "video.air.updated_at_ms",
        "video.air.encoder_hw_accel",
        "video.air.cloud_branch_open",
    ]
    return RouteSpec(
        name="air-pipeline-metrics",
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


def _air_pipeline_state_event_route() -> RouteSpec:
    """Air-pipeline string fields, carried on the air-state snapshot event."""
    return RouteSpec(
        name="air-pipeline-state",
        kind="events",
        logd_params={"kind": "events", "limit": 200},
        observability_path="/api/v2/observability/events",
        row_match={"kind": "video.air_state"},
        fields=[
            FieldSpec(
                field="pipeline_state",
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-logd",
            ),
            FieldSpec(
                field="encoder_name",
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-logd",
            ),
            FieldSpec(
                field="camera_source",
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-logd",
            ),
        ],
    )


def _video_latency_metrics_route() -> RouteSpec:
    """SEI glass-to-glass latency fields the latency route reads back."""
    names = [
        "video.latency.glass_ms",
        "video.latency.ewma_ms",
        "video.latency.pipeline_ms",
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
