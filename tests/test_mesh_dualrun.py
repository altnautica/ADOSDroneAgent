"""Value-parity for the logd-backed ground-station mesh read source.

The mesh read source reads the durable logging store and falls back to the
live ``mesh-state.json`` sidecar. These tests assert the store-derived snapshot
and its slicer projections survive the msgpack round-trip and degrade to None
when the store is unreachable or empty, exercising the legs most likely to
drift: the neighbors→routes alias, the nested neighbor/gateway arrays, the
selected gateway, and the empty/absent-snapshot path.
"""

from __future__ import annotations

from unittest.mock import patch

import httpx
import pytest

from ados.api import telemetry_source
from ados.api.sources import mesh as mesh_source

# A full mesh-state body matching the producer keys exactly, with two neighbors,
# one selected gateway, and the dormant ``partition`` / ``started_at_ms`` fields
# at their poll-loop defaults (false / 0) so the test carries them honestly.
_FULL_MESH = {
    "role": "receiver",
    "bat_iface": "bat0",
    "mesh_iface": "wlan1",
    "carrier": "802.11s",
    "mesh_id": "ados-abc",
    "up": True,
    "neighbors": [
        {"mac": "aa:bb:cc:dd:ee:ff", "iface": "wlan1", "tq": 240, "last_seen_ms": 1234},
        {"mac": "11:22:33:44:55:66", "iface": "wlan1", "tq": 200, "last_seen_ms": 999},
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
}


def _events_envelope(detail):
    data = []
    if detail is not None:
        data = [
            {
                "id": 1,
                "ts_us": 1_000,
                "session": 7,
                "kind": "mesh.state",
                "source": "ados-groundlink",
                "severity": "info",
                "detail": detail,
            }
        ]
    return {"data": data, "page": {"count": len(data)}, "meta": {"source": "logd"}}


def _mesh_client(detail):
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/v1/query"
        assert request.url.params.get("kind") == "events"
        assert request.url.params.get("event_kind") == "mesh.state"
        return httpx.Response(200, json=_events_envelope(detail))

    return httpx.AsyncClient(
        base_url="http://logd", transport=httpx.MockTransport(handler)
    )


# --- source helper round-trips ----------------------------------------------


@pytest.mark.asyncio
async def test_latest_mesh_snapshot_returns_the_body():
    """The source returns the full stored detail map for the newest event."""
    with patch.object(telemetry_source, "_get_client", lambda: _mesh_client(_FULL_MESH)):
        got = await mesh_source.latest_mesh_snapshot()
    assert got == _FULL_MESH


@pytest.mark.asyncio
async def test_latest_mesh_snapshot_store_down_returns_none():
    """An unreachable store yields None so the route falls back to the sidecar."""

    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("store down")

    client = httpx.AsyncClient(
        base_url="http://logd", transport=httpx.MockTransport(handler)
    )
    with patch.object(telemetry_source, "_get_client", lambda: client):
        assert await mesh_source.latest_mesh_snapshot() is None


@pytest.mark.asyncio
async def test_latest_mesh_snapshot_empty_store_returns_none():
    """No mesh.state event yields None so the route reads the sidecar."""
    with patch.object(telemetry_source, "_get_client", lambda: _mesh_client(None)):
        assert await mesh_source.latest_mesh_snapshot() is None


def test_slicers_match_the_live_projection():
    """Each slicer projects the same shape the live route slices off the file."""
    assert mesh_source.slice_neighbors(_FULL_MESH) == {
        "neighbors": _FULL_MESH["neighbors"]
    }
    # routes are aliased to neighbors today — the alias must hold on both paths.
    assert mesh_source.slice_routes(_FULL_MESH) == {
        "routes": _FULL_MESH["neighbors"]
    }
    assert mesh_source.slice_gateways(_FULL_MESH) == {
        "gateways": _FULL_MESH["gateways"],
        "selected": "11:22:33:44:55:66",
    }


