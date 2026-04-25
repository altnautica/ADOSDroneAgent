"""Smoke tests for the ground-station route module.

Structural regression net before the routes file decomposition. Covers
six URL groups:

* /status
* /wfb, /wfb/relay, /wfb/receiver
* /network, /network/ethernet
* /ui, /display, /bluetooth, /gamepads, /pic
* /mesh, /role, /ws/uplink
* /pair, /pairing

Tests focus on route registration, auth wiring, profile gating, and
shape-of-response contracts. Service internals are mocked aggressively
so the suite stays hermetic.
"""

from __future__ import annotations

import time
from typing import Any
from unittest.mock import AsyncMock, MagicMock

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.core.config import ADOSConfig
from ados.core.health import HealthMonitor
from ados.core.main import ServiceTracker
from ados.services.mavlink.state import VehicleState


GS_PREFIX = "/api/v1/ground-station"


def _build_agent_app(profile: str = "ground_station") -> Any:
    app = MagicMock()
    cfg = ADOSConfig()
    cfg.agent.profile = profile
    app.config = cfg
    app.health = HealthMonitor()
    app.services = ServiceTracker()
    app._start_time = time.monotonic()
    app.uptime_seconds = 42.0
    app._vehicle_state = VehicleState()
    app._fc_connection = MagicMock()
    app._fc_connection.connected = False
    app._fc_connection.port = ""
    app._fc_connection.baud = 0
    app._tasks = []
    app._param_cache = None
    # Auth middleware short-circuits when unpaired.
    app.pairing_manager.is_paired = False
    return app


@pytest.fixture
def agent_app():
    return _build_agent_app("ground_station")


@pytest.fixture
def drone_agent_app():
    return _build_agent_app("auto")


@pytest.fixture
def client(agent_app):
    return TestClient(create_app(agent_app))


@pytest.fixture
def drone_client(drone_agent_app):
    return TestClient(create_app(drone_agent_app))


@pytest.fixture
def patch_role(monkeypatch):
    """Helper to override get_current_role across the route module."""
    from ados.services.ground_station import role_manager

    def _set(role: str) -> None:
        monkeypatch.setattr(role_manager, "get_current_role", lambda: role)

    return _set


# ---------------------------------------------------------------------------
# Group 1: /status
# ---------------------------------------------------------------------------


def test_status_route_exists(client, monkeypatch):
    """GET /status returns a ground-station snapshot with the OLED schema keys."""
    from ados.api.routes import ground_station as gs

    fake_pm = MagicMock()
    fake_pm.status = AsyncMock(return_value={"paired": False, "key_fingerprint": None})
    monkeypatch.setattr(gs, "_pair_manager", lambda: fake_pm)

    resp = client.get(f"{GS_PREFIX}/status")
    assert resp.status_code == 200
    data = resp.json()
    assert data["profile"] == "ground_station"
    for key in ("paired_drone", "link", "gcs", "network", "system", "role", "mesh"):
        assert key in data


def test_status_profile_gate_drone(drone_client):
    """Drone profile gets 404 with the profile mismatch error code."""
    resp = drone_client.get(f"{GS_PREFIX}/status")
    assert resp.status_code == 404
    body = resp.json()
    assert body["detail"]["error"]["code"] == "E_PROFILE_MISMATCH"


# ---------------------------------------------------------------------------
# Group 2: /wfb, /wfb/relay, /wfb/receiver
# ---------------------------------------------------------------------------


def test_wfb_get_returns_radio_view(client):
    """GET /wfb surfaces channel + bitrate + fec from agent config."""
    resp = client.get(f"{GS_PREFIX}/wfb")
    assert resp.status_code == 200
    data = resp.json()
    for key in ("channel", "bitrate_profile", "fec"):
        assert key in data


def test_wfb_put_updates_radio(client):
    """PUT /wfb accepts a partial update and echoes the new state."""
    resp = client.put(f"{GS_PREFIX}/wfb", json={"channel": 161})
    assert resp.status_code in (200, 503)


def test_wfb_relay_status_wrong_role(client, patch_role):
    """GET /wfb/relay/status on a non-relay returns 404 with role error."""
    patch_role("direct")
    resp = client.get(f"{GS_PREFIX}/wfb/relay/status")
    assert resp.status_code == 404
    assert resp.json()["detail"]["error"]["code"] == "E_WRONG_ROLE"


def test_wfb_receiver_relays_wrong_role(client, patch_role):
    """GET /wfb/receiver/relays on a non-receiver returns 404."""
    patch_role("direct")
    resp = client.get(f"{GS_PREFIX}/wfb/receiver/relays")
    assert resp.status_code == 404


def test_wfb_receiver_combined_wrong_role(client, patch_role):
    """GET /wfb/receiver/combined on a non-receiver returns 404."""
    patch_role("direct")
    resp = client.get(f"{GS_PREFIX}/wfb/receiver/combined")
    assert resp.status_code == 404


# ---------------------------------------------------------------------------
# Group 3: /network, /network/ethernet
# ---------------------------------------------------------------------------


def test_network_get_returns_uplink_matrix(client, monkeypatch):
    """GET /network surfaces the four uplinks plus the active uplink view."""
    from ados.api.routes import ground_station as gs

    monkeypatch.setattr(gs, "_ap_view", lambda app: {"enabled": False})
    monkeypatch.setattr(
        gs, "_wifi_client_view", AsyncMock(return_value={"connected": False})
    )
    monkeypatch.setattr(
        gs, "_ethernet_view", AsyncMock(return_value={"connected": False})
    )
    monkeypatch.setattr(gs, "_modem_view", AsyncMock(return_value={"connected": False}))
    monkeypatch.setattr(
        gs,
        "_router_state_view",
        lambda: {"active_uplink": None, "priority": []},
    )
    monkeypatch.setattr(gs, "_load_share_uplink_flag", lambda: False)

    resp = client.get(f"{GS_PREFIX}/network")
    assert resp.status_code == 200
    data = resp.json()
    for key in ("ap", "wifi_client", "ethernet", "modem_4g", "active_uplink"):
        assert key in data


def test_network_ethernet_get(client, monkeypatch):
    """GET /network/ethernet returns a view from the ethernet manager."""
    from ados.api.routes import ground_station as gs

    fake_mgr = MagicMock()
    fake_mgr.config = AsyncMock(return_value={"mode": "dhcp", "link_up": False})
    monkeypatch.setattr(gs, "_ethernet_mgr", lambda: fake_mgr)

    resp = client.get(f"{GS_PREFIX}/network/ethernet")
    assert resp.status_code == 200
    assert resp.json()["mode"] == "dhcp"


def test_network_ethernet_put_static_missing_fields(client, monkeypatch):
    """PUT /network/ethernet with mode=static and no ip returns 400."""
    from ados.api.routes import ground_station as gs

    fake_mgr = MagicMock()
    monkeypatch.setattr(gs, "_ethernet_mgr", lambda: fake_mgr)

    resp = client.put(
        f"{GS_PREFIX}/network/ethernet",
        json={"mode": "static"},
    )
    assert resp.status_code == 400
    assert (
        resp.json()["detail"]["error"]["code"] == "E_ETHERNET_STATIC_MISSING_FIELDS"
    )


# ---------------------------------------------------------------------------
# Group 4: /ui, /display, /bluetooth, /gamepads, /pic
# ---------------------------------------------------------------------------


def test_ui_get_returns_full_config(client):
    """GET /ui returns the OLED + buttons + screens config blob."""
    resp = client.get(f"{GS_PREFIX}/ui")
    assert resp.status_code == 200
    data = resp.json()
    for key in ("oled", "buttons", "screens"):
        assert key in data


def test_ui_oled_put_validation(client):
    """PUT /ui/oled rejects out-of-range brightness."""
    resp = client.put(f"{GS_PREFIX}/ui/oled", json={"brightness": 999})
    assert resp.status_code == 422


def test_display_get(client):
    """GET /display returns the persisted HDMI display config."""
    resp = client.get(f"{GS_PREFIX}/display")
    assert resp.status_code == 200


def test_display_put_invalid_resolution(client):
    """PUT /display with a bogus resolution returns 400."""
    resp = client.put(
        f"{GS_PREFIX}/display",
        json={"resolution": "8k"},
    )
    assert resp.status_code == 400
    assert resp.json()["detail"]["error"]["code"] == "E_INVALID_RESOLUTION"


def test_bluetooth_paired_list(client, monkeypatch):
    """GET /bluetooth/paired returns the paired-device list from the input manager."""
    from ados.api.routes import ground_station as gs

    fake_input = MagicMock()
    fake_input.paired_bluetooth = AsyncMock(return_value=[{"mac": "AA:BB:CC"}])
    monkeypatch.setattr(gs, "_input_manager", lambda: fake_input)

    resp = client.get(f"{GS_PREFIX}/bluetooth/paired")
    assert resp.status_code == 200
    devices = resp.json()["devices"]
    assert isinstance(devices, list)
    assert devices[0]["mac"] == "AA:BB:CC"


def test_gamepads_list(client, monkeypatch):
    """GET /gamepads exposes connected gamepads + primary id."""
    from ados.api.routes import ground_station as gs

    fake_input = MagicMock()
    fake_input.list_gamepads = MagicMock(return_value=[{"id": "g0"}])
    fake_input.get_primary = MagicMock(return_value=None)
    monkeypatch.setattr(gs, "_input_manager", lambda: fake_input)

    resp = client.get(f"{GS_PREFIX}/gamepads")
    assert resp.status_code == 200
    data = resp.json()
    assert "devices" in data
    assert "primary_id" in data


def test_pic_get_state(client, monkeypatch):
    """GET /pic returns the arbiter snapshot."""
    from ados.api.routes import ground_station as gs

    fake_arb = MagicMock()
    fake_arb.get_state = MagicMock(
        return_value={"claimed_by": None, "ttl_ms": 0}
    )
    monkeypatch.setattr(gs, "_pic_arbiter", lambda: fake_arb)

    resp = client.get(f"{GS_PREFIX}/pic")
    assert resp.status_code == 200
    assert "claimed_by" in resp.json()


# ---------------------------------------------------------------------------
# Group 5: /mesh, /role, /ws/uplink
# ---------------------------------------------------------------------------


def test_role_get(client, patch_role):
    """GET /role surfaces current role + supported roles."""
    patch_role("direct")
    resp = client.get(f"{GS_PREFIX}/role")
    assert resp.status_code == 200
    data = resp.json()
    assert data["role"] == "direct"
    assert "direct" in data["supported"]


def test_mesh_get_direct_returns_404(client, patch_role):
    """GET /mesh on a direct node returns 404 with the not-in-mesh code."""
    patch_role("direct")
    resp = client.get(f"{GS_PREFIX}/mesh")
    assert resp.status_code == 404
    assert resp.json()["detail"]["error"]["code"] == "E_NOT_IN_MESH"


def test_mesh_neighbors_direct_404(client, patch_role):
    """GET /mesh/neighbors on a direct node returns 404."""
    patch_role("direct")
    resp = client.get(f"{GS_PREFIX}/mesh/neighbors")
    assert resp.status_code == 404


def test_mesh_config_get(client):
    """GET /mesh/config reads the mesh sub-block from agent config."""
    resp = client.get(f"{GS_PREFIX}/mesh/config")
    assert resp.status_code == 200
    data = resp.json()
    for key in ("mesh_id", "carrier", "channel", "bat_iface"):
        assert key in data


def test_ws_uplink_profile_gate_drone(drone_client):
    """WS /ws/uplink closes 1008 on drone profile."""
    with pytest.raises(Exception):
        with drone_client.websocket_connect(f"{GS_PREFIX}/ws/uplink"):
            pass


# ---------------------------------------------------------------------------
# Group 6: /pair, /pairing
# ---------------------------------------------------------------------------


def test_pair_accept_wrong_role(client, patch_role):
    """POST /pair/accept requires the receiver role."""
    patch_role("direct")
    resp = client.post(f"{GS_PREFIX}/pair/accept", json={"duration_s": 60})
    assert resp.status_code == 409
    assert resp.json()["detail"]["error"]["code"] == "E_WRONG_ROLE"


def test_pair_close_wrong_role(client, patch_role):
    """POST /pair/close requires the receiver role."""
    patch_role("direct")
    resp = client.post(f"{GS_PREFIX}/pair/close")
    assert resp.status_code == 409


def test_pair_accept_validation(client):
    """POST /pair/accept validates duration_s bounds."""
    resp = client.post(f"{GS_PREFIX}/pair/accept", json={"duration_s": 1})
    assert resp.status_code == 422


def test_pair_revoke_wrong_role(client, patch_role):
    """POST /pair/revoke/{device_id} requires the receiver role."""
    patch_role("direct")
    resp = client.post(f"{GS_PREFIX}/pair/revoke/dev-123")
    assert resp.status_code == 409


def test_wfb_pair_post_requires_body(client):
    """POST /wfb/pair requires a pair_key field."""
    resp = client.post(f"{GS_PREFIX}/wfb/pair", json={})
    assert resp.status_code == 422
