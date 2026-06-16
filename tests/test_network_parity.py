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

from ados.api.sources import network as network_source

# The active-uplink snapshot the ados-net daemon ships on each flag change.
_UPLINK_ACTIVE_BLOB: dict[str, Any] = {
    "active_uplink": "eth0",
    "internet_reachable": True,
    "timestamp_ms": 1_717_000_000_000,
    "data_cap_state": "ok",
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


def _empty_query_rows():
    async def fake_query_rows(kind, limit, **params):
        return None

    return fake_query_rows
