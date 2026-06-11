"""Deterministic dual-run checks against mocked store and legacy responses.

Every transport here is an ``httpx.MockTransport`` so the comparison logic is
exercised end-to-end with no live service and no network, on any host (macOS
included).
"""

import httpx
from logd_conformance.client import Fetcher
from logd_conformance.routes import initial_routes
from logd_conformance.runner import run_conformance, run_route


def _logd_client(rows_by_kind, reachable=True):
    def handler(request):
        if not reachable:
            return httpx.Response(503)
        if request.url.path == "/v1/query":
            kind = request.url.params.get("kind")
            data = rows_by_kind.get(kind, [])
            return httpx.Response(
                200, json={"data": data, "page": {"count": len(data)}, "meta": {}}
            )
        return httpx.Response(404)

    return httpx.Client(
        transport=httpx.MockTransport(handler), base_url="http://logd.local"
    )


def _legacy_client(log_entries):
    def handler(request):
        if request.url.path == "/api/logs":
            return httpx.Response(
                200, json={"entries": log_entries, "total": len(log_entries)}
            )
        # Observability proxy is not wired yet → 404, which the fetcher reads as
        # unreachable (None), the expected pre-cutover state.
        return httpx.Response(404)

    return httpx.Client(
        transport=httpx.MockTransport(handler), base_url="http://legacy.local"
    )


_FULL_METRICS = [
    {"metric": "link.rssi_dbm", "value": -53.0},
    {"metric": "link.snr_db", "value": 12.0},
    {"metric": "link.fec_uncorrected", "value": 0.0},
    {"metric": "link.loss_percent", "value": 0.25},
    {"metric": "link.bitrate_kbps", "value": 5700.0},
    {"metric": "video.encoder_bitrate_kbps", "value": 4000.0},
    {"metric": "video.framerate_hz", "value": 30.0},
    {"metric": "video.queue_depth_frames", "value": 0.0},
    {"metric": "video.dropped_frames_cumulative", "value": 0.0},
    {"metric": "video.air.encoder_fps", "value": 30.0},
    {"metric": "video.air.encoded_kbps", "value": 6000.0},
    {"metric": "video.air.sei_injected_count", "value": 12.0},
    {"metric": "video.air.udp_bytes_out", "value": 4096.0},
    {"metric": "video.air.restart_count", "value": 1.0},
    {"metric": "video.air.tx_silent_kicks", "value": 0.0},
    {"metric": "video.air.bus_errors", "value": 0.0},
    {"metric": "video.air.updated_at_ms", "value": 1717000000000.0},
    {"metric": "video.air.encoder_hw_accel", "value": 1.0},
    {"metric": "video.air.cloud_branch_open", "value": 0.0},
    {"metric": "video.latency.glass_ms", "value": 42.5},
    {"metric": "video.latency.ewma_ms", "value": 40.1},
    {"metric": "video.latency.pipeline_ms", "value": 0.0},
    {"metric": "video.latency.samples", "value": 7.0},
    {"metric": "cpu.utilization_pct", "value": 12.0},
    {"metric": "mem.available_pct", "value": 75.0},
    {"metric": "disk.used_pct", "value": 40.0},
    {"metric": "thermal.primary_c", "value": 48.0},
]

_FULL_LOGS = [
    {"id": 1, "ts_us": 123, "level": "INFO", "source": "ados-radio", "msg": "hi"}
]

_FULL_HW = [
    {
        "id": 1,
        "ts_us": 123,
        "signals": {
            "mem.total_bytes": 4_000_000_000,
            "mem.avail_bytes": 1_000_000_000,
            "mem.cache_bytes": 500_000_000,
            "mem.swap_total_bytes": 1_000_000_000,
            "disk.fs_total_bytes": 32_000_000_000,
            "disk.fs_used_bytes": 8_000_000_000,
            "sched.loadavg_1": 0.5,
            "thermal.primary_c": 48.0,
            "cpu.util.all": 12.0,
        },
    }
]

_SERVICE_EVENT = {
    "id": 1,
    "ts_us": 1,
    "kind": "service.transition",
    "source": "ados-supervisor",
    "severity": "info",
    "detail": {
        "service": "ados-video",
        "from_state": "stopped",
        "to_state": "running",
        "reason": "start_ok",
    },
}

_AIR_STATE_EVENT = {
    "id": 2,
    "ts_us": 2,
    "kind": "video.air_state",
    "source": "sidecar-tap",
    "severity": "info",
    "detail": {
        "name": "air-pipeline.json",
        "camera_source": "v4l2src",
        "encoder_name": "v4l2h264enc",
        "pipeline_state": "playing",
    },
}

# The full air-side wfb-status body the radio ships each heartbeat: every field
# the wfb-status-drone route checks is a key here (the link-quality spine + the
# transmit-plane truth: pair identity, the adaptive-bitrate controller intent,
# the watchdog counters, the USB-speed gate, the reg posture).
_WFB_STATUS_DRONE_EVENT = {
    "id": 3,
    "ts_us": 5,
    "kind": "link.wfb_status",
    "source": "ados-radio",
    "severity": "info",
    "detail": {
        "state": "connected",
        "link_state": "connected",
        "interface": "wlan1",
        "channel": 161,
        "actual_channel": 161,
        "rendezvous_channel": 149,
        "operating_channel": 157,
        "reg_domain": "US",
        "reg_verified": True,
        "enabled_channels": [149, 153, 157, 161, 165],
        "regPosture": "unrestricted",
        "pinnedRegion": None,
        "regVerified": True,
        "rf_unverified": False,
        "adapter_chipset": "RTL8812EU",
        "adapter_injection_ok": True,
        "adapter_usb_speed_mbps": 480,
        "adapter_usb_degraded": False,
        "tx_power_dbm": 5,
        "tx_power_max_dbm": 15,
        "topology": "host_vbus",
        "mcs_index": 1,
        "fec_k": 8,
        "fec_n": 12,
        "channel_locked": True,
        "profile": "drone",
        "restart_count": 0,
        "paired": True,
        "auto_pair_enabled": True,
        "tx_zombie_kills": 0,
        "phy_muted": False,
        "tx_bytes_per_s": 187234.0,
        "valid_rx_packets_per_s": 815.0,
        "link_preset": "balanced",
        "adaptive_bitrate_enabled": True,
        "recommended_bitrate_kbps": 6000,
        "rssi_dbm": -53.0,
        "rssi_min": -58.0,
        "rssi_max": -49.0,
        "noise_dbm": -95.0,
        "snr_db": 42.0,
        "packets_received": 1200,
        "packets_lost": 3,
        "fec_recovered": 2,
        "fec_failed": 0,
        "bitrate_kbps": 5700,
        "loss_percent": 0.25,
        "timestamp": "2026-06-10T00:00:05Z",
    },
}

# The full ground-side wfb-status body the GS receiver ships: the link-quality
# spine plus the receive-plane truth (acquire state, reacquire/zombie kills, the
# inbound-video rate, the rx-silence window).
_WFB_STATUS_GS_EVENT = {
    "id": 4,
    "ts_us": 6,
    "kind": "link.wfb_status",
    "source": "ados-groundlink",
    "severity": "info",
    "detail": {
        "state": "active",
        "link_state": "active",
        "interface": "wlan1",
        "channel": 157,
        "actual_channel": 157,
        "rendezvous_channel": 149,
        "operating_channel": 149,
        "reg_domain": "US",
        "reg_verified": True,
        "enabled_channels": [149, 153, 157, 161, 165],
        "rf_unverified": False,
        "adapter_chipset": "rtl88x2eu",
        "adapter_injection_ok": True,
        "tx_power_dbm": 5,
        "tx_power_max_dbm": 15,
        "topology": "host_vbus",
        "mcs_index": 1,
        "channel_locked": True,
        "profile": "ground_station",
        "acquire_state": "locked",
        "valid_rx_packets_per_s": 12.5,
        "reacquire_kills": 2,
        "rx_zombie_kills": 1,
        "video_inbound_bytes_per_s": 508000.0,
        "rx_silent_seconds": 0.3,
        "rssi_dbm": -53.0,
        "noise_dbm": -95.0,
        "snr_db": 42.0,
        "packets_received": 1200,
        "packets_lost": 3,
        "fec_recovered": 2,
        "fec_failed": 0,
        "bitrate_kbps": 5700,
        "loss_percent": 0.25,
        "timestamp": "2026-06-10T00:00:06Z",
    },
}

_WFB_FAILOVER_EVENT = {
    "id": 5,
    "ts_us": 7,
    "kind": "wfb.pair.failover",
    "source": "ados-supervisor",
    "severity": "info",
    "detail": {"state": "local"},
}

_FULL_EVENTS = [
    _SERVICE_EVENT,
    _AIR_STATE_EVENT,
    _WFB_STATUS_DRONE_EVENT,
    _WFB_STATUS_GS_EVENT,
    _WFB_FAILOVER_EVENT,
]

_LEGACY_LOGS = [
    {
        "seq": 1,
        "timestamp": "2026-06-05T00:00:00+00:00",
        "level": "INFO",
        "logger": "ados-radio",
        "message": "hi",
    }
]


def test_full_superset_passes_every_field():
    rows = {
        "logs": _FULL_LOGS,
        "metrics": _FULL_METRICS,
        "events": _FULL_EVENTS,
        "hw": _FULL_HW,
    }
    fetcher = Fetcher([_logd_client(rows)], _legacy_client(_LEGACY_LOGS))
    report = run_conformance(fetcher, initial_routes())
    assert report.ok
    assert report.failed == 0
    assert report.missing_producer == 0
    # The log route documents that the store is a superset of legacy: every
    # mapped legacy field was served by the legacy handler too.
    log_route = next(r for r in report.routes if r.route == "logs")
    assert log_route.legacy_reachable is True
    assert all(f.legacy_present for f in log_route.fields)


def test_missing_metrics_producer_is_reported_not_failed():
    # Store reachable, but the metrics table is empty: every metric field is a
    # missing producer, not a schema gap. The run still passes (no fail) unless
    # strict mode is requested.
    rows = {"logs": _FULL_LOGS, "metrics": [], "events": [_SERVICE_EVENT]}
    fetcher = Fetcher([_logd_client(rows)], _legacy_client(_LEGACY_LOGS))
    report = run_conformance(fetcher, initial_routes())
    assert report.ok
    assert report.failed == 0
    assert report.missing_producer >= 3  # at least the link metrics
    link = next(r for r in report.routes if r.route == "link-metrics")
    assert all(f.status == "missing-producer" for f in link.fields)

    strict = run_conformance(fetcher, initial_routes(), strict=True)
    assert not strict.ok


def test_event_detail_schema_gap_fails():
    # The supervisor emits service.transition rows but the detail lacks
    # ``reason``: a genuine schema gap → fail (not missing-producer).
    bad_event = {
        "kind": "service.transition",
        "source": "ados-supervisor",
        "detail": {"from_state": "stopped", "to_state": "running"},
    }
    rows = {"logs": _FULL_LOGS, "metrics": _FULL_METRICS, "events": [bad_event]}
    fetcher = Fetcher([_logd_client(rows)], _legacy_client(_LEGACY_LOGS))
    report = run_conformance(fetcher, initial_routes())
    assert not report.ok
    assert report.failed == 1
    svc = next(r for r in report.routes if r.route == "service-events")
    reason = next(f for f in svc.fields if f.field == "reason")
    assert reason.status == "fail"
    # The present detail keys still pass.
    assert next(f for f in svc.fields if f.field == "from_state").status == "pass"


def test_event_kind_filter_excludes_other_events():
    # An events table that has rows but none of kind service.transition: the
    # producer is not emitting our event → missing-producer, never a false pass.
    other = {"kind": "radio.lock", "detail": {"x": 1}}
    rows = {"logs": _FULL_LOGS, "metrics": _FULL_METRICS, "events": [other]}
    fetcher = Fetcher([_logd_client(rows)], _legacy_client(_LEGACY_LOGS))
    result = run_route(fetcher, next(r for r in initial_routes() if r.name == "service-events"))
    assert result.logd_rows == 1
    assert result.matched_rows == 0
    assert all(f.status == "missing-producer" for f in result.fields)


def test_unreachable_store_marks_all_missing_producer():
    fetcher = Fetcher([_logd_client({}, reachable=False)], _legacy_client(_LEGACY_LOGS))
    report = run_conformance(fetcher, initial_routes())
    assert report.failed == 0
    assert report.passed == 0
    for route in report.routes:
        assert route.logd_reachable is False
        assert all(f.status == "missing-producer" for f in route.fields)


def test_socket_first_then_tcp_fallback():
    # The first store client is unreachable (socket down); the second answers
    # (the TCP fallback). The run must still see the rows.
    rows = {
        "logs": _FULL_LOGS,
        "metrics": _FULL_METRICS,
        "events": _FULL_EVENTS,
    }
    fetcher = Fetcher(
        [_logd_client({}, reachable=False), _logd_client(rows)],
        _legacy_client(_LEGACY_LOGS),
    )
    report = run_conformance(fetcher, initial_routes())
    assert report.ok
    assert report.failed == 0
