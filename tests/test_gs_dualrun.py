"""Value-parity for the logd-backed relay/receiver read routes + /status mesh.

The relay/receiver read routes (``/wfb/relay/status``, ``/wfb/receiver/relays``,
``/wfb/receiver/combined``) read the durable logging store first and fall back
to the live ``wfb-relay.json`` / ``wfb-receiver.json`` sidecars. These tests
assert the store-derived response is byte-identical to the live-fallback one,
exercising the legs most likely to drift: the relay body returned verbatim, the
``receiver_ip: null`` surviving the msgpack round-trip, the nested ``relays``
array, and the combined projection's per-key defaults.

The ``/status`` ``mesh`` sub-block is store-backed off the same ``mesh.state``
event and gets a parity check too; the rest of ``/status`` (pair / AP probe /
psutil / recorder / role / link) is live-only and untouched here.
"""

from __future__ import annotations

from unittest.mock import patch

import httpx
import pytest

from ados.api import telemetry_source
from ados.api.routes.ground_station import wfb as wfb_routes
from ados.api.sources import gs as gs_source
from tests._parity import assert_route_parity

# A full relay-state body (the wfb-relay.json shape), with the null receiver_ip
# leg exercised.
_FULL_RELAY = {
    "role": "relay",
    "drone_iface": "wlan1",
    "receiver_ip": None,
    "receiver_port": 5800,
    "receiver_last_seen_ms": 0,
    "fragments_seen": 12345,
    "fragments_forwarded": 12000,
    "up": False,
    "mesh_iface": "bat0",
}

# A full receiver-state body (the wfb-receiver.json shape), with a 2-entry relays
# array.
_FULL_RECEIVER = {
    "role": "receiver",
    "drone_iface": "wlan1",
    "listen_port": 5800,
    "accept_local_nic": True,
    "mesh_iface": "bat0",
    "relays": [
        {"mac": "aa:bb:cc:dd:ee:ff", "last_seen_ms": 1_717_000_000_000, "fragments": 4096},
        {"mac": "11:22:33:44:55:66", "last_seen_ms": 1_717_000_000_500, "fragments": 2048},
    ],
    "fragments_after_dedup": 8000,
    "fec_repaired": 24,
    "output_kbps": 4200,
    "up": True,
}


def _events_envelope(kind, detail):
    data = []
    if detail is not None:
        data = [
            {
                "id": 1,
                "ts_us": 1_000,
                "session": 7,
                "kind": kind,
                "source": "ados-groundlink",
                "severity": "info",
                "detail": detail,
            }
        ]
    return {"data": data, "page": {"count": len(data)}, "meta": {"source": "logd"}}


def _event_client(kind, detail):
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/v1/query"
        assert request.url.params.get("kind") == "events"
        assert request.url.params.get("event_kind") == kind
        return httpx.Response(200, json=_events_envelope(kind, detail))

    return httpx.AsyncClient(
        base_url="http://logd", transport=httpx.MockTransport(handler)
    )


# --- source helper round-trips ----------------------------------------------


@pytest.mark.asyncio
async def test_latest_relay_state_round_trips_with_null_receiver_ip():
    """The relay source returns the full body verbatim, null receiver_ip intact."""
    with patch.object(
        telemetry_source, "_get_client", lambda: _event_client("gs.relay_state", _FULL_RELAY)
    ):
        got = await gs_source.latest_relay_state()
    assert got == _FULL_RELAY
    assert got["receiver_ip"] is None


@pytest.mark.asyncio
async def test_latest_receiver_state_round_trips_with_relays_array():
    """The receiver source returns the full body, the nested relays array intact."""
    with patch.object(
        telemetry_source,
        "_get_client",
        lambda: _event_client("gs.receiver_state", _FULL_RECEIVER),
    ):
        got = await gs_source.latest_receiver_state()
    assert got == _FULL_RECEIVER
    assert len(got["relays"]) == 2


@pytest.mark.asyncio
async def test_relay_state_store_down_returns_none():
    """An unreachable store yields None so the route falls back to the sidecar."""

    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("store down")

    client = httpx.AsyncClient(
        base_url="http://logd", transport=httpx.MockTransport(handler)
    )
    with patch.object(telemetry_source, "_get_client", lambda: client):
        assert await gs_source.latest_relay_state() is None
        assert await gs_source.latest_receiver_state() is None


def test_receiver_projections_match_the_live_route():
    """The relays + combined slicers project the same shapes the live route does."""
    assert gs_source.slice_receiver_relays(_FULL_RECEIVER) == {
        "relays": _FULL_RECEIVER["relays"]
    }
    assert gs_source.slice_receiver_combined(_FULL_RECEIVER) == {
        "fragments_after_dedup": 8000,
        "fec_repaired": 24,
        "output_kbps": 4200,
        "up": True,
    }
    # Per-key defaults coalesce identically when keys are omitted.
    assert gs_source.slice_receiver_combined({}) == {
        "fragments_after_dedup": 0,
        "fec_repaired": 0,
        "output_kbps": 0,
        "up": False,
    }
    assert gs_source.slice_receiver_relays({}) == {"relays": []}


# --- end-to-end route parity ------------------------------------------------


@pytest.mark.asyncio
async def test_relay_status_route_dual_run_parity(tmp_path):
    """/wfb/relay/status returns the same dict from the store or the sidecar."""
    sidecar = tmp_path / "wfb-relay.json"
    import json

    sidecar.write_text(json.dumps(_FULL_RELAY), encoding="utf-8")
    with (
        patch.object(wfb_routes._gs, "_require_ground_profile", lambda: None),
        patch(
            "ados.services.ground_station.role_manager.get_current_role",
            return_value="relay",
        ),
        patch.object(wfb_routes._gs, "_WFB_RELAY_JSON", sidecar),
    ):
        await assert_route_parity(
            call_route=wfb_routes.get_wfb_relay_status,
            source_target="ados.api.sources.gs.latest_relay_state",
            logd_signals=_FULL_RELAY,
            ignore_fields=set(),
        )


@pytest.mark.asyncio
async def test_receiver_routes_dual_run_parity(tmp_path):
    """The two receiver routes return the same dict from the store or the sidecar."""
    sidecar = tmp_path / "wfb-receiver.json"
    import json

    sidecar.write_text(json.dumps(_FULL_RECEIVER), encoding="utf-8")
    with (
        patch.object(wfb_routes._gs, "_require_ground_profile", lambda: None),
        patch(
            "ados.services.ground_station.role_manager.get_current_role",
            return_value="receiver",
        ),
        patch.object(wfb_routes._gs, "_WFB_RECEIVER_JSON", sidecar),
    ):
        for call_route in (
            wfb_routes.get_wfb_receiver_relays,
            wfb_routes.get_wfb_receiver_combined,
        ):
            await assert_route_parity(
                call_route=call_route,
                source_target="ados.api.sources.gs.latest_receiver_state",
                logd_signals=_FULL_RECEIVER,
                ignore_fields=set(),
            )


@pytest.mark.asyncio
async def test_relay_status_store_down_falls_back_to_sidecar(tmp_path):
    """When the store is down the relay route reads the sidecar, never 500s."""
    sidecar = tmp_path / "wfb-relay.json"
    import json

    sidecar.write_text(json.dumps(_FULL_RELAY), encoding="utf-8")
    with (
        patch.object(wfb_routes._gs, "_require_ground_profile", lambda: None),
        patch(
            "ados.services.ground_station.role_manager.get_current_role",
            return_value="relay",
        ),
        patch.object(wfb_routes._gs, "_WFB_RELAY_JSON", sidecar),
        patch("ados.api.sources.gs.latest_relay_state", return_value=None),
    ):
        out = await wfb_routes.get_wfb_relay_status()
    assert out == _FULL_RELAY


# --- /status mesh sub-block --------------------------------------------------

_FULL_MESH = {
    "role": "receiver",
    "bat_iface": "bat0",
    "mesh_iface": "wlan1",
    "carrier": "802.11s",
    "mesh_id": "ados-abc",
    "up": True,
    "neighbors": [
        {"mac": "aa:bb:cc:dd:ee:ff", "iface": "wlan1", "tq": 240, "last_seen_ms": 1234},
    ],
    "gateways": [],
    "selected_gateway": None,
    "partition": False,
    "started_at_ms": 0,
    "last_poll_ms": 1_700_000_000_000,
}


@pytest.mark.asyncio
async def test_status_mesh_block_matches_the_live_projection():
    """The /status mesh sub-block projects the same five fields the live route
    reads off the sidecar, with identical bool/len coercions."""
    with patch(
        "ados.api.sources.gs.latest_mesh_snapshot",
        return_value=_FULL_MESH,
    ):
        block = await gs_source.latest_status_mesh_block()
    assert block == {
        "up": True,
        "peer_count": 1,
        "selected_gateway": None,
        "partition": False,
        "mesh_id": "ados-abc",
    }


@pytest.mark.asyncio
async def test_status_mesh_block_store_down_returns_none():
    """No mesh.state event yields None so /status falls back to the sidecar read."""
    with patch(
        "ados.api.sources.gs.latest_mesh_snapshot",
        return_value=None,
    ):
        assert await gs_source.latest_status_mesh_block() is None
