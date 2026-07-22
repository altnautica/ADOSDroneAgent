"""Conformance specs for the link-quality, wfb-status, and failover routes."""

from __future__ import annotations

from ..routes import FieldSpec, Locator, RouteSpec

# The status-body keys both the air-side (ados-radio) and the ground-side
# (ados-groundlink) producers emit, plus the keys each emits alone. The combined
# route check splits drone-vs-gs by source so a single-profile rig reports the
# other profile's exclusives as missing-producer (no rows for that source) rather
# than as a schema gap on the rows it does have. The adapter USB keys are common:
# a slow-USB adapter strands a receiver as surely as a transmitter, so both
# planes report the link speed and the degraded verdict.
_WFB_STATUS_COMMON = [
    "state",
    "link_state",
    "interface",
    "channel",
    "actual_channel",
    "rendezvous_channel",
    "operating_channel",
    "reg_domain",
    "reg_verified",
    "enabled_channels",
    "rf_unverified",
    "adapter_chipset",
    "adapter_injection_ok",
    "adapter_usb_speed_mbps",
    "adapter_usb_degraded",
    "tx_power_dbm",
    "tx_power_max_dbm",
    "topology",
    "mcs_index",
    "channel_locked",
    "profile",
    "rssi_dbm",
    "noise_dbm",
    "snr_db",
    "packets_received",
    "packets_lost",
    "fec_recovered",
    "fec_failed",
    "bitrate_kbps",
    "loss_percent",
    "timestamp",
]

# Air-side-only status keys (the transmit-plane truth: pair identity, the
# adaptive-bitrate controller intent, the watchdog counters).
_WFB_STATUS_DRONE_ONLY = [
    "regPosture",
    "pinnedRegion",
    "regVerified",
    "fec_k",
    "fec_n",
    "restart_count",
    "paired",
    "auto_pair_enabled",
    "tx_zombie_kills",
    "phy_muted",
    "tx_bytes_per_s",
    "valid_rx_packets_per_s",
    "link_preset",
    "adaptive_bitrate_enabled",
    "recommended_bitrate_kbps",
    "rssi_min",
    "rssi_max",
]

# Ground-side-only status keys (the receive-plane truth: acquire state, the
# reacquire/zombie kills, the inbound-video rate, the rx-silence window).
_WFB_STATUS_GS_ONLY = [
    "acquire_state",
    "valid_rx_packets_per_s",
    "reacquire_kills",
    "rx_zombie_kills",
    "video_inbound_bytes_per_s",
    "rx_silent_seconds",
]


def routes() -> list[RouteSpec]:
    """The link-quality + wfb-status + failover route set."""
    return [
        _link_metrics_route(),
        _wfb_status_drone_route(),
        _wfb_status_gs_route(),
        _wfb_history_route(),
        _wfb_failover_route(),
    ]


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


def _wfb_status_drone_route() -> RouteSpec:
    """Air-side full wfb-status body (live events, store-only).

    The /api/wfb route reads this back instead of the sidecar file; every field
    is a key in the shipped detail map. Filtered to the air-side source so the
    ground-only keys never count against it.
    """
    fields = _WFB_STATUS_COMMON + _WFB_STATUS_DRONE_ONLY
    return RouteSpec(
        name="wfb-status-drone",
        kind="events",
        logd_params={"kind": "events", "event_kind": "link.wfb_status", "limit": 200},
        observability_path="/api/v2/observability/events",
        row_match={"kind": "link.wfb_status", "source": "ados-radio"},
        fields=[
            FieldSpec(
                field=f,
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-radio",
            )
            for f in fields
        ],
    )


def _wfb_status_gs_route() -> RouteSpec:
    """Ground-side full wfb-status body (live events, store-only).

    The GS /api/wfb route reads this back instead of the sidecar file. Filtered
    to the ground-side source so the air-only keys never count against it.
    """
    fields = _WFB_STATUS_COMMON + _WFB_STATUS_GS_ONLY
    return RouteSpec(
        name="wfb-status-gs",
        kind="events",
        logd_params={"kind": "events", "event_kind": "link.wfb_status", "limit": 200},
        observability_path="/api/v2/observability/events",
        row_match={"kind": "link.wfb_status", "source": "ados-groundlink"},
        fields=[
            FieldSpec(
                field=f,
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-groundlink",
            )
            for f in fields
        ],
    )


def _wfb_history_route() -> RouteSpec:
    """Durable link-quality history samples (live telemetry, store-only).

    The /api/wfb/history route reshapes these aggregated metrics into its sample
    list. rssi/snr already ship under the link-metrics route; loss/bitrate are
    the samples that round out the history shape.
    """
    return RouteSpec(
        name="wfb-history",
        kind="metrics",
        logd_params={"kind": "metrics", "limit": 200},
        observability_path="/api/v2/observability/metrics",
        fields=[
            FieldSpec(
                field=m,
                locator=Locator.METRIC,
                classification="live",
                producer="ados-radio|ados-groundlink",
            )
            for m in [
                "link.rssi_dbm",
                "link.snr_db",
                "link.loss_percent",
                "link.bitrate_kbps",
            ]
        ],
    )


def _wfb_failover_route() -> RouteSpec:
    """Local-bind to cloud-relay failover transitions (live events, store-only)."""
    return RouteSpec(
        name="wfb-failover",
        kind="events",
        logd_params={"kind": "events", "event_kind": "wfb.pair.failover", "limit": 200},
        observability_path="/api/v2/observability/events",
        row_match={"kind": "wfb.pair.failover"},
        fields=[
            FieldSpec(
                field="state",
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-supervisor",
            ),
        ],
    )
