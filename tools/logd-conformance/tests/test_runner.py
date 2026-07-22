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
    {
        "metric": "service.memory_pss_bytes",
        "value": 83046400.0,
        "tags": {"unit": "ados-api.service"},
    },
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
        "adapter_usb_speed_mbps": 480,
        "adapter_usb_degraded": False,
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

# The full mesh snapshot body the relay/receiver poll loop ships: every key the
# mesh-state route checks (the four mesh slice routes read this back).
_MESH_STATE_EVENT = {
    "id": 6,
    "ts_us": 8,
    "kind": "mesh.state",
    "source": "ados-groundlink",
    "severity": "info",
    "detail": {
        "role": "receiver",
        "bat_iface": "bat0",
        "mesh_iface": "wlan1",
        "carrier": "802.11s",
        "mesh_id": "ados-abc",
        "up": True,
        "neighbors": [
            {"mac": "aa:bb:cc:dd:ee:ff", "iface": "wlan1", "tq": 240, "last_seen_ms": 1234},
        ],
        "gateways": [
            {
                "mac": "11:22:33:44:55:66",
                "class_up_kbps": 10000,
                "class_down_kbps": 2000,
                "tq": 255,
                "selected": True,
            },
        ],
        "selected_gateway": "11:22:33:44:55:66",
        "partition": False,
        "started_at_ms": 0,
        "last_poll_ms": 1_700_000_000_000,
    },
}

# The full relay state body the relay loop ships: every key the gs-relay-state
# route checks (the /wfb/relay/status route reads this back verbatim).
_GS_RELAY_STATE_EVENT = {
    "id": 7,
    "ts_us": 9,
    "kind": "gs.relay_state",
    "source": "ados-groundlink",
    "severity": "info",
    "detail": {
        "role": "relay",
        "drone_iface": "wlan1",
        "receiver_ip": "10.42.0.5",
        "receiver_port": 5800,
        "receiver_last_seen_ms": 1_717_000_000_000,
        "fragments_seen": 12345,
        "fragments_forwarded": 12000,
        "up": True,
        "mesh_iface": "bat0",
    },
}

# The full receiver state body the receiver loop ships: every key the
# gs-receiver-state route checks (the relays + combined routes project subsets).
_GS_RECEIVER_STATE_EVENT = {
    "id": 8,
    "ts_us": 10,
    "kind": "gs.receiver_state",
    "source": "ados-groundlink",
    "severity": "info",
    "detail": {
        "role": "receiver",
        "drone_iface": "wlan1",
        "listen_port": 5800,
        "accept_local_nic": True,
        "mesh_iface": "bat0",
        "relays": [
            {"mac": "aa:bb:cc:dd:ee:ff", "last_seen_ms": 1_717_000_000_000, "fragments": 4096},
        ],
        "fragments_after_dedup": 8000,
        "fec_repaired": 24,
        "output_kbps": 4200,
        "up": True,
    },
}

# The full active-uplink snapshot the network daemon ships: every key the
# net-uplink-active route checks.
_NET_UPLINK_ACTIVE_EVENT = {
    "id": 9,
    "ts_us": 11,
    "kind": "net.uplink_active",
    "source": "ados-net",
    "severity": "info",
    "detail": {
        "active_uplink": "wifi",
        "internet_reachable": True,
        "timestamp_ms": 1_717_000_000_000,
        "data_cap_state": "ok",
    },
}

# The full data-cap usage block the network daemon ships: every key the
# net-modem-usage route checks.
_NET_MODEM_USAGE_EVENT = {
    "id": 10,
    "ts_us": 12,
    "kind": "net.modem_usage",
    "source": "ados-net",
    "severity": "info",
    "detail": {
        "data_used_mb": 512,
        "cap_mb": 10240,
        "percent": 5.0,
        "state": "ok",
        "window_reset_at": "2026-07-01T00:00:00Z",
        "last_reset_month": "2026-06",
    },
}

_FULL_EVENTS = [
    _SERVICE_EVENT,
    _AIR_STATE_EVENT,
    _WFB_STATUS_DRONE_EVENT,
    _WFB_STATUS_GS_EVENT,
    _WFB_FAILOVER_EVENT,
    _MESH_STATE_EVENT,
    _GS_RELAY_STATE_EVENT,
    _GS_RECEIVER_STATE_EVENT,
    _NET_UPLINK_ACTIVE_EVENT,
    _NET_MODEM_USAGE_EVENT,
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
