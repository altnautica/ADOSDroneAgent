"""Value-parity for the logd-backed /api/wfb, /api/wfb/history, and the pair
failover-status routes.

Each route reads the durable logging store first and falls back to the live
source (the /run/ados sidecar files / the empty native history). These tests
assert the store-derived response is byte-identical to the live-fallback one for
the same underlying data, exercising the legs most likely to drift: the
effective-vs-configured tx_power, the mcs/topology null-coalesce, the
frequency/bandwidth re-derivation, the always-live regulatory_domain override,
the bitrate_mbps shim, the msgpack-fragile null + array round-trip, and the
event-age staleness flip.
"""

from __future__ import annotations

import time
from unittest.mock import patch

import httpx
import pytest

from ados.api import telemetry_source
from ados.api.routes import wfb as wfb_routes
from ados.api.sources import wfb as wfb_source
from ados.core.config import ADOSConfig

# A full air-side wfb-status body, the exact value ados-radio's build_stats_value
# emits, carrying the hard cases: an effective tx_power (5) distinct from the
# config default (15), a live channel (161) that re-derives 5805/20, a null
# pinnedRegion + a channel array (the msgpack-fragile legs), and a non-zero
# bitrate that drives the bitrate_mbps shim.
_FULL_BODY = {
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
    "paired_with_device_id": "dev-abc",
    "paired_at": "2026-06-10T00:00:00Z",
    "public_key_fingerprint": "00112233aabbccdd",
    "auto_pair_enabled": True,
    "tx_zombie_kills": 0,
    "tx_video_stalled": False,
    "tx_video_stall_kills": 0,
    "tx_video_recvq_bytes": 0,
    "phy_muted": False,
    "tx_bytes_per_s": 187234.0,
    "valid_rx_packets_per_s": 815.0,
    "link_preset": "balanced",
    "adaptive_bitrate_enabled": True,
    "recommended_bitrate_kbps": 6000,
    "recommended_tier_idx": 2,
    "recommended_tier_name": "rung2",
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
}


def _live_from_body(body: dict, wfb_cfg) -> dict:
    """Run the live read over ``body`` written to a fresh temp sidecar.

    Patches WFB_STATS_JSON to a temp file with a current mtime (so the staleness
    flip does not trip) and returns what _build_status_from_stats_file produces.
    """
    import json
    import os
    import tempfile

    fd, path = tempfile.mkstemp(suffix=".json")
    try:
        with os.fdopen(fd, "w") as fh:
            json.dump(body, fh)
        with patch("ados.core.paths.WFB_STATS_JSON", __import__("pathlib").Path(path)):
            return wfb_routes._build_status_from_stats_file(wfb_cfg)
    finally:
        os.unlink(path)


def _wfb_cfg():
    """A default config's video.wfb block (tx_power_dbm 15, mcs/topology set)."""
    return ADOSConfig().video.wfb


@patch.object(wfb_routes, "_read_regulatory_domain", return_value="US")
def test_status_parity_full_body(_reg):
    """The store-derived /api/wfb equals the live read for the full body.

    Covers effective tx_power (5 from the body, not the config 15), the
    frequency/bandwidth re-derivation (161 → 5805/20), the bitrate_mbps shim
    (5700 → 5.7), and the null pinnedRegion + channel array surviving intact.
    """
    cfg = _wfb_cfg()
    live = _live_from_body(_FULL_BODY, cfg)
    # ts_us is now-ish so the event-age staleness check does not flip it.
    ts_us = int(time.time() * 1_000_000)
    derived = wfb_source.derive_wfb_status(dict(_FULL_BODY), ts_us, cfg)
    assert derived == live
    # Spot the hard cases explicitly.
    assert derived["tx_power_dbm"] == 5  # effective, overrides config 15
    assert derived["frequency_mhz"] == 5805
    assert derived["bandwidth_mhz"] == 20
    assert derived["bitrate_mbps"] == 5.7
    assert derived["regulatory_domain"] == "US"
    assert derived["pinnedRegion"] is None
    assert derived["enabled_channels"] == [149, 153, 157, 161, 165]


@patch.object(wfb_routes, "_read_regulatory_domain", return_value="US")
def test_status_parity_omitted_fields_fall_to_config(_reg):
    """A body that omits tx_power/mcs/topology falls to the config base on both
    paths, so the null-coalesce stays identical."""
    cfg = _wfb_cfg()
    body = dict(_FULL_BODY)
    for key in ("tx_power_dbm", "tx_power_max_dbm", "topology", "mcs_index"):
        body.pop(key, None)
    live = _live_from_body(body, cfg)
    derived = wfb_source.derive_wfb_status(
        dict(body), int(time.time() * 1_000_000), cfg
    )
    assert derived == live
    # The config defaults seed these when the body omits them.
    assert derived["tx_power_dbm"] == cfg.tx_power_dbm
    assert derived["tx_power_max_dbm"] == cfg.tx_power_max_dbm
    assert derived["topology"] == cfg.topology
    assert derived["mcs_index"] == cfg.mcs_index


@patch.object(wfb_routes, "_read_regulatory_domain", return_value="US")
def test_status_parity_unknown_channel_zeroes_frequency(_reg):
    """An unknown channel re-derives 0/0 frequency on both paths."""
    cfg = _wfb_cfg()
    body = dict(_FULL_BODY)
    body["channel"] = 0
    live = _live_from_body(body, cfg)
    derived = wfb_source.derive_wfb_status(
        dict(body), int(time.time() * 1_000_000), cfg
    )
    assert derived == live
    assert derived["frequency_mhz"] == 0
    assert derived["bandwidth_mhz"] == 0


@patch.object(wfb_routes, "_read_regulatory_domain", return_value="US")
def test_status_parity_zero_bitrate_shim(_reg):
    """A zero bitrate yields bitrate_mbps 0.0 on both paths (not a crash)."""
    cfg = _wfb_cfg()
    body = dict(_FULL_BODY)
    body["bitrate_kbps"] = 0
    live = _live_from_body(body, cfg)
    derived = wfb_source.derive_wfb_status(
        dict(body), int(time.time() * 1_000_000), cfg
    )
    assert derived == live
    assert derived["bitrate_mbps"] == 0.0


@patch.object(wfb_routes, "_read_regulatory_domain", return_value="US")
def test_status_regulatory_domain_is_always_live(_reg):
    """The body's reg_domain never leaks into regulatory_domain: the live probe
    wins on both paths."""
    cfg = _wfb_cfg()
    body = dict(_FULL_BODY)
    body["reg_domain"] = "BO"  # the radio's wanted domain, a distinct key
    live = _live_from_body(body, cfg)
    derived = wfb_source.derive_wfb_status(
        dict(body), int(time.time() * 1_000_000), cfg
    )
    assert derived == live
    # regulatory_domain is the live US, reg_domain stays the body's BO.
    assert derived["regulatory_domain"] == "US"
    assert derived["reg_domain"] == "BO"


@patch.object(wfb_routes, "_read_regulatory_domain", return_value="US")
def test_status_stale_when_event_is_old(_reg):
    """An event older than the staleness window flips state to ``stale``,
    matching the live read's mtime>10s flip."""
    cfg = _wfb_cfg()
    old_ts = int((time.time() - 30) * 1_000_000)  # 30 s old
    derived = wfb_source.derive_wfb_status(dict(_FULL_BODY), old_ts, cfg)
    assert derived["state"] == "stale"
    # A fresh event keeps the body's own state.
    fresh = wfb_source.derive_wfb_status(
        dict(_FULL_BODY), int(time.time() * 1_000_000), cfg
    )
    assert fresh["state"] == "connected"


# --- history reshape ---------------------------------------------------------


@pytest.mark.asyncio
async def test_history_reshapes_aggregate_buckets_into_samples():
    """latest_wfb_history groups per-metric aggregate buckets into one sample per
    bucket instant, keyed to the route's sample shape."""

    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/v1/aggregate"
        # Two bucket instants, each carrying the four link metrics.
        data = [
            {"bucket_us": 1_000_000, "metric": "link.rssi_dbm", "value": -53.0, "count": 1},
            {"bucket_us": 1_000_000, "metric": "link.snr_db", "value": 42.0, "count": 1},
            {"bucket_us": 1_000_000, "metric": "link.loss_percent", "value": 0.25, "count": 1},
            {"bucket_us": 1_000_000, "metric": "link.bitrate_kbps", "value": 5700.0, "count": 1},
            {"bucket_us": 2_000_000, "metric": "link.rssi_dbm", "value": -55.0, "count": 1},
            {"bucket_us": 2_000_000, "metric": "link.snr_db", "value": 40.0, "count": 1},
            {"bucket_us": 2_000_000, "metric": "link.loss_percent", "value": 0.5, "count": 1},
            {"bucket_us": 2_000_000, "metric": "link.bitrate_kbps", "value": 5600.0, "count": 1},
        ]
        return httpx.Response(200, json={"data": data})

    client = httpx.AsyncClient(
        base_url="http://logd", transport=httpx.MockTransport(handler)
    )
    with patch.object(wfb_source, "_get_client", lambda: client):
        hist = await wfb_source.latest_wfb_history(60)
    assert hist is not None
    assert hist["count"] == 2
    s0, s1 = hist["samples"]
    assert s0["rssi_dbm"] == -53.0
    assert s0["snr_db"] == 42.0
    assert s0["loss_percent"] == 0.25
    assert s0["bitrate_kbps"] == 5700.0
    assert "timestamp" in s0
    assert s1["rssi_dbm"] == -55.0
    # Samples are ordered by bucket instant.
    assert s0["timestamp"] < s1["timestamp"]


@pytest.mark.asyncio
async def test_history_store_down_returns_none_so_route_falls_back():
    """An unreachable store returns None so the route falls back to the native
    empty history."""

    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("store down")

    client = httpx.AsyncClient(
        base_url="http://logd", transport=httpx.MockTransport(handler)
    )
    with patch.object(wfb_source, "_get_client", lambda: client):
        assert await wfb_source.latest_wfb_history(60) is None


# --- failover ---------------------------------------------------------------


def _events_envelope(state):
    data = []
    if state is not None:
        data = [
            {
                "id": 1,
                "ts_us": 1_000,
                "session": 7,
                "kind": "wfb.pair.failover",
                "source": "ados-supervisor",
                "severity": "warn" if state == "cloud_relay" else "info",
                "detail": {"state": state},
            }
        ]
    return {"data": data, "page": {"count": len(data)}, "meta": {"source": "logd"}}


def _failover_client(state):
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/v1/query"
        assert request.url.params.get("kind") == "events"
        assert request.url.params.get("event_kind") == "wfb.pair.failover"
        return httpx.Response(200, json=_events_envelope(state))

    return httpx.AsyncClient(
        base_url="http://logd", transport=httpx.MockTransport(handler)
    )


@pytest.mark.asyncio
@pytest.mark.parametrize("state", ["local", "cloud_relay", "failed"])
async def test_failover_helper_returns_validated_state(state):
    """For every state the live route can return, the helper returns the same
    validated string, so the route's wrapped dict matches the live sidecar.

    The helper reads via ``telemetry_source.query_rows``, which uses the shared
    client in ``telemetry_source`` — so the mock is installed there.
    """
    with patch.object(
        telemetry_source, "_get_client", lambda: _failover_client(state)
    ):
        got = await wfb_source.latest_wfb_failover()
    assert got == state
    assert {"failover_state": got} == {"failover_state": state}


@pytest.mark.asyncio
async def test_failover_unknown_state_falls_back():
    """An unrecognized state yields None so the route reads the sidecar."""
    with patch.object(
        telemetry_source, "_get_client", lambda: _failover_client("garbage")
    ):
        assert await wfb_source.latest_wfb_failover() is None


@pytest.mark.asyncio
async def test_failover_empty_store_falls_back():
    """No failover event yields None so the route reads the sidecar."""
    with patch.object(
        telemetry_source, "_get_client", lambda: _failover_client(None)
    ):
        assert await wfb_source.latest_wfb_failover() is None
