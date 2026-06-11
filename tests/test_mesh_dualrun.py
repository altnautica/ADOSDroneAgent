"""Value-parity for the logd-backed ground-station mesh read routes.

The four mesh read routes (``/mesh``, ``/mesh/neighbors``, ``/mesh/routes``,
``/mesh/gateways``) read the durable logging store first and fall back to the
live ``mesh-state.json`` sidecar. These tests assert the store-derived response
is byte-identical to the live-fallback one for the same underlying snapshot,
exercising the legs most likely to drift: the neighbors→routes alias, the nested
neighbor/gateway arrays surviving the msgpack round-trip, the null
``selected_gateway``, and the empty/absent-snapshot degrade-to-fallback path.

The ``/role`` and ``/mesh/config`` routes are LIVE-ONLY (config-file + Pydantic
config + systemd unit-name constants), so they are NOT migrated and carry no
store source; a guard test below asserts the mesh source is never touched by
``/role``.
"""

from __future__ import annotations

from unittest.mock import patch

import httpx
import pytest

from ados.api import telemetry_source
from ados.api.routes.ground_station import mesh as mesh_routes
from ados.api.sources import mesh as mesh_source
from tests._parity import assert_route_parity

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


# --- end-to-end route parity ------------------------------------------------


def _patch_gates():
    """Patch the live profile + role gates so handlers reach the store branch."""
    return (
        patch.object(mesh_routes._gs, "_require_ground_profile", lambda: None),
        patch(
            "ados.services.ground_station.role_manager.get_current_role",
            return_value="receiver",
        ),
    )


@pytest.mark.asyncio
async def test_mesh_route_dual_run_parity(tmp_path):
    """The four snapshot routes return the same dict whether they read the store
    or the sidecar, for the same underlying body."""
    sidecar = tmp_path / "mesh-state.json"
    import json

    sidecar.write_text(json.dumps(_FULL_MESH), encoding="utf-8")

    routes = [
        mesh_routes.get_mesh_health,
        mesh_routes.get_mesh_neighbors,
        mesh_routes.get_mesh_routes,
        mesh_routes.get_mesh_gateways,
    ]
    prof_patch, role_patch = _patch_gates()
    with (
        prof_patch,
        role_patch,
        patch.object(mesh_routes._gs, "_MESH_STATE_JSON", sidecar),
    ):
        for call_route in routes:
            await assert_route_parity(
                call_route=call_route,
                source_target="ados.api.sources.mesh.latest_mesh_snapshot",
                logd_signals=_FULL_MESH,
                ignore_fields=set(),
            )


@pytest.mark.asyncio
async def test_mesh_route_empty_snapshot_degrades_identically(tmp_path):
    """With no store event and a missing sidecar, both paths degrade to the empty
    shape, never to a 500."""
    missing = tmp_path / "absent.json"  # never created
    prof_patch, role_patch = _patch_gates()
    with (
        prof_patch,
        role_patch,
        patch.object(mesh_routes._gs, "_MESH_STATE_JSON", missing),
        patch(
            "ados.api.sources.mesh.latest_mesh_snapshot",
            return_value=None,
        ),
    ):
        assert await mesh_routes.get_mesh_health() == {}
        assert await mesh_routes.get_mesh_neighbors() == {"neighbors": []}
        assert await mesh_routes.get_mesh_routes() == {"routes": []}
        assert await mesh_routes.get_mesh_gateways() == {
            "gateways": [],
            "selected": None,
        }


# --- live-only guard --------------------------------------------------------


@pytest.mark.asyncio
async def test_role_route_is_live_only_and_never_touches_the_store(monkeypatch):
    """/role is served from the role sentinel + config + unit constants, never
    from the mesh store. The mesh source must not be imported/called by it."""
    from ados.api.routes.ground_station import mesh as m

    # A ground-profile facade with a ground_station config block.
    class _Cfg:
        class agent:  # noqa: N801
            profile = "ground_station"

        class ground_station:  # noqa: N801
            role = "relay"

    class _App:
        config = _Cfg()

    monkeypatch.setattr(m._gs, "_require_ground_profile", lambda: _App())
    monkeypatch.setattr(
        "ados.services.ground_station.role_manager.get_current_role",
        lambda: "relay",
    )
    monkeypatch.setattr(
        "ados.services.ground_station.role_manager.role_units",
        lambda role: ["ados-wfb-relay.service"],
    )
    monkeypatch.setattr(
        "ados.services.ground_station.role_manager.all_mesh_units",
        lambda: ["ados-wfb-relay.service", "ados-wfb-receiver.service"],
    )

    called = {"hit": False}

    async def _boom():
        called["hit"] = True
        return None

    monkeypatch.setattr(mesh_source, "latest_mesh_snapshot", _boom)

    out = await m.get_role()
    assert out["role"] == "relay"
    assert out["configured"] == "relay"
    assert out["supported"] == ["direct", "relay", "receiver"]
    assert called["hit"] is False
