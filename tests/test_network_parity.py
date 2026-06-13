"""Value-parity between the live network reads and their logd-derived twins.

The aggregate ``GET /network`` view and the ``GET /network/modem`` view now read
the store-backable legs (the failover-selected ``active_uplink`` and the
cumulative data-cap usage figures) from the durable store first, falling back to
the live in-process read. These tests prove the two paths return the IDENTICAL
response, and that the live-only legs (the AP / wifi / ethernet probes, the live
modem connectivity ``state`` / iface / signal, the priority config file) are
untouched.

The store is mocked at the ``query_rows`` seam in the ``network`` source module,
so the helpers run their real mapping over canned event rows.
"""

from __future__ import annotations

from typing import Any
from unittest.mock import AsyncMock, MagicMock

import pytest

from ados.api.routes.ground_station import network as network_route
from ados.api.sources import network as network_source

# The active-uplink snapshot the ados-net daemon ships on each flag change.
_UPLINK_ACTIVE_BLOB: dict[str, Any] = {
    "active_uplink": "eth0",
    "internet_reachable": True,
    "timestamp_ms": 1_717_000_000_000,
    "data_cap_state": "ok",
}

# The cumulative data-cap usage block the daemon ships each poll. Note `state`
# here is the cap classification, NOT the modem connectivity state the view
# surfaces, so the view never reads it.
_MODEM_USAGE_BLOB: dict[str, Any] = {
    "data_used_mb": 321,
    "cap_mb": 5120,
    "percent": 6.27,
    "state": "ok",
    "window_reset_at": 1_716_000_000.0,
    "last_reset_month": "2026-05",
}


def _patch_store(monkeypatch, *, event_rows: list[dict[str, Any]]) -> None:
    """Make query_rows in the network source return canned event rows.

    Mirrors the real helper: an events query filters the canned rows by
    ``event_kind`` (the row's ``kind``); anything else returns None.
    """

    async def fake_query_rows(kind, limit, **params):
        if kind == "events":
            wanted = params.get("event_kind")
            return [r for r in event_rows if r.get("kind") == wanted] or None
        return None

    monkeypatch.setattr(network_source, "query_rows", fake_query_rows)


# --------------------------------------------------------------------------- #
# /network active_uplink parity
# --------------------------------------------------------------------------- #


def _patch_network_legs(monkeypatch, *, live_active_uplink: str | None) -> None:
    """Pin all the /network legs except active_uplink so only it can differ."""
    from ados.api.routes import ground_station as gs

    monkeypatch.setattr(network_route._gs, "_require_ground_profile", lambda: MagicMock())
    monkeypatch.setattr(gs, "_ap_view", lambda app: {"enabled": False})
    monkeypatch.setattr(gs, "_wifi_client_view", AsyncMock(return_value={"connected": False}))
    monkeypatch.setattr(gs, "_ethernet_view", AsyncMock(return_value={"connected": False}))
    monkeypatch.setattr(gs, "_modem_view", AsyncMock(return_value={"connected": False}))
    monkeypatch.setattr(
        gs,
        "_router_state_view",
        lambda: {"active_uplink": live_active_uplink, "priority": ["eth0", "wlan0_client"]},
    )
    monkeypatch.setattr(gs, "_load_share_uplink_flag", lambda: False)


@pytest.mark.asyncio
async def test_network_active_uplink_logd_matches_live(monkeypatch):
    # Live and store agree on the selected uplink -> the two paths are identical.
    _patch_network_legs(monkeypatch, live_active_uplink="eth0")

    _patch_store(monkeypatch, event_rows=[])
    live = await network_route.get_ground_station_network()

    _patch_store(
        monkeypatch,
        event_rows=[{"kind": "net.uplink_active", "detail": _UPLINK_ACTIVE_BLOB}],
    )
    derived = await network_route.get_ground_station_network()

    assert derived == live
    assert derived["active_uplink"] == "eth0"
    # The other legs are untouched live reads.
    assert derived["priority"] == ["eth0", "wlan0_client"]
    assert derived["share_uplink"] is False


@pytest.mark.asyncio
async def test_network_active_uplink_comes_from_the_store_not_the_singleton(monkeypatch):
    # The whole point: the in-process singleton is dead-on-read (None) while the
    # daemon's store event carries the real selected uplink. The store value
    # must win, so the view reflects the daemon's truth.
    _patch_network_legs(monkeypatch, live_active_uplink=None)
    _patch_store(
        monkeypatch,
        event_rows=[{"kind": "net.uplink_active", "detail": _UPLINK_ACTIVE_BLOB}],
    )
    derived = await network_route.get_ground_station_network()
    assert derived["active_uplink"] == "eth0"


@pytest.mark.asyncio
async def test_network_active_uplink_null_store_form_round_trips(monkeypatch):
    # The daemon's unlink (no-uplink) form carries active_uplink: None. A
    # store-first reader must surface None, identical to a live no-uplink view.
    _patch_network_legs(monkeypatch, live_active_uplink=None)
    null_blob = dict(_UPLINK_ACTIVE_BLOB, active_uplink=None, internet_reachable=False)
    _patch_store(
        monkeypatch,
        event_rows=[{"kind": "net.uplink_active", "detail": null_blob}],
    )
    derived = await network_route.get_ground_station_network()
    assert derived["active_uplink"] is None


@pytest.mark.asyncio
async def test_network_falls_back_to_live_when_store_empty(monkeypatch):
    # No store event -> the route falls back to the live in-process view, never
    # a 500.
    _patch_network_legs(monkeypatch, live_active_uplink="wlan0_client")
    _patch_store(monkeypatch, event_rows=[])
    derived = await network_route.get_ground_station_network()
    assert derived["active_uplink"] == "wlan0_client"


# --------------------------------------------------------------------------- #
# /network/modem usage-block parity
# --------------------------------------------------------------------------- #


def _stub_modem_manager(monkeypatch, *, live_used_mb: int, live_cap_mb: int) -> None:
    """Stub the modem manager so only the usage figures can differ.

    The connectivity legs (connected/iface/signal/state) are pinned identical
    across the live and store paths so the comparison isolates the usage block.
    """
    monkeypatch.setattr(network_route._gs, "_require_ground_profile", lambda: MagicMock())

    mgr = MagicMock()
    mgr.status = AsyncMock(
        return_value={
            "connected": True,
            "iface": "wwan0",
            "ip": "10.0.0.5",
            "signal_quality": 72,
            "technology": "lte",
            "apn": "internet",
            "operator": "TestNet",
        }
    )
    # total_bytes drives the live data_used_mb figure.
    mgr.data_usage = AsyncMock(return_value={"total_bytes": live_used_mb * 1024 * 1024})
    mgr._config = {"enabled": True, "cap_gb": live_cap_mb / 1024.0, "apn": "internet"}

    monkeypatch.setattr(
        "ados.services.ground_station.modem_manager.get_modem_manager",
        lambda: mgr,
    )


@pytest.mark.asyncio
async def test_modem_usage_logd_matches_live(monkeypatch):
    # The live data_usage figures already equal the store figures, so the two
    # paths are byte-identical on every field.
    _stub_modem_manager(monkeypatch, live_used_mb=321, live_cap_mb=5120)

    _patch_store(monkeypatch, event_rows=[])
    live = await network_route.get_network_modem()

    _patch_store(
        monkeypatch,
        event_rows=[{"kind": "net.modem_usage", "detail": _MODEM_USAGE_BLOB}],
    )
    derived = await network_route.get_network_modem()

    assert derived == live
    assert derived["data_used_mb"] == 321
    assert derived["cap_mb"] == 5120
    assert derived["percent"] == 6.27
    # The connectivity `state` is the live modem connectivity, NOT the store's
    # cap classification — it stays "connected".
    assert derived["state"] == "connected"
    assert derived["iface"] == "wwan0"


@pytest.mark.asyncio
async def test_modem_usage_comes_from_the_store_when_live_counter_is_blind(monkeypatch):
    # The FastAPI box can't see the modem iface, so the live counter reads 0.
    # The store event carries the daemon's real cumulative figures, which must
    # win — the connectivity legs still come from the live status.
    _stub_modem_manager(monkeypatch, live_used_mb=0, live_cap_mb=5120)
    _patch_store(
        monkeypatch,
        event_rows=[{"kind": "net.modem_usage", "detail": _MODEM_USAGE_BLOB}],
    )
    derived = await network_route.get_network_modem()
    assert derived["data_used_mb"] == 321
    assert derived["percent"] == 6.27
    assert derived["state"] == "connected"


@pytest.mark.asyncio
async def test_modem_usage_falls_back_to_live_when_store_empty(monkeypatch):
    # No store event -> the live data_usage()-derived figures are used.
    _stub_modem_manager(monkeypatch, live_used_mb=100, live_cap_mb=5120)
    _patch_store(monkeypatch, event_rows=[])
    derived = await network_route.get_network_modem()
    assert derived["data_used_mb"] == 100
    assert derived["cap_mb"] == 5120


# --------------------------------------------------------------------------- #
# helper-level units (the pure source mapping)
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_latest_uplink_active_none_without_producer(monkeypatch):
    _patch_store(monkeypatch, event_rows=[])
    assert await network_source.latest_uplink_active() is None


@pytest.mark.asyncio
async def test_latest_modem_usage_none_without_producer(monkeypatch):
    _patch_store(monkeypatch, event_rows=[])
    assert await network_source.latest_modem_usage() is None


@pytest.mark.asyncio
async def test_latest_uplink_active_returns_the_detail_body(monkeypatch):
    _patch_store(
        monkeypatch,
        event_rows=[{"kind": "net.uplink_active", "detail": _UPLINK_ACTIVE_BLOB}],
    )
    assert await network_source.latest_uplink_active() == _UPLINK_ACTIVE_BLOB


# --------------------------------------------------------------------------- #
# share_uplink: resolve the active iface store-first, no lying applied:true
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_share_uplink_apply_reports_no_active_uplink(monkeypatch):
    # The native daemon shipped no active-uplink event, so the store resolves no
    # iface. There is nothing to MASQUERADE on, so the helper must report the
    # honest not-applied result with a `reason`, never a lying applied:true.
    from ados.api.routes.ground_station._common import share_uplink as su

    monkeypatch.setattr(network_source, "query_rows", _empty_query_rows())
    result = await su._apply_share_uplink(True)
    assert result["applied"] is False
    assert result["reason"] == "no_active_uplink"
    # apply_error carries the same cause for older GCS builds that read it.
    assert result["apply_error"] == "no_active_uplink"


@pytest.mark.asyncio
async def test_share_uplink_apply_resolves_iface_from_store(monkeypatch):
    # The store carries the daemon's selected uplink name; the helper maps it to
    # a kernel iface via the router's stateless name->iface helper and hands
    # that to the firewall apply. A live firewall result of applied:true passes
    # through unchanged.
    from ados.api.routes.ground_station._common import share_uplink as su

    _patch_store(
        monkeypatch,
        event_rows=[{"kind": "net.uplink_active", "detail": _UPLINK_ACTIVE_BLOB}],
    )

    fake_router = MagicMock()
    fake_router._uplink_iface = AsyncMock(return_value="eth0")
    monkeypatch.setattr(su, "_uplink_router", lambda: fake_router)

    applied_with: dict[str, Any] = {}

    async def fake_apply(enabled, iface):
        applied_with["enabled"] = enabled
        applied_with["iface"] = iface
        return {"applied": True, "apply_error": None, "backend": "iptables-persistent"}

    monkeypatch.setattr(
        "ados.services.ground_station.share_uplink_firewall.apply_share_uplink",
        fake_apply,
    )

    result = await su._apply_share_uplink(True)
    assert result["applied"] is True
    assert result["backend"] == "iptables-persistent"
    # The active uplink "eth0" was mapped to its kernel iface and passed through.
    assert applied_with == {"enabled": True, "iface": "eth0"}


@pytest.mark.asyncio
async def test_put_share_uplink_route_surfaces_reason(monkeypatch):
    # End-to-end through the route: persist succeeds, the apply reports
    # no_active_uplink, and the route surfaces `applied:false` + the `reason`.
    from ados.api.routes.ground_station import network as network_route_mod
    from ados.api.routes.ground_station._common import models

    monkeypatch.setattr(
        network_route_mod._gs, "_require_ground_profile", lambda: MagicMock()
    )
    monkeypatch.setattr(
        network_route_mod._gs, "_persist_share_uplink_flag", lambda enabled: None
    )

    async def fake_apply(enabled):
        return {
            "applied": False,
            "reason": "no_active_uplink",
            "apply_error": "no_active_uplink",
            "backend": None,
        }

    monkeypatch.setattr(network_route_mod._gs, "_apply_share_uplink", fake_apply)

    res = await network_route_mod.put_network_share_uplink(
        models.ShareUplinkUpdate(enabled=True)
    )
    assert res["enabled"] is True
    assert res["applied"] is False
    assert res["reason"] == "no_active_uplink"


def _empty_query_rows():
    async def fake_query_rows(kind, limit, **params):
        return None

    return fake_query_rows
