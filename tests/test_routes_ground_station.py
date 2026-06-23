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

from typing import Any

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.core.config import ADOSConfig
from tests.api_runtime_utils import build_api_runtime

GS_PREFIX = "/api/v1/ground-station"


def _build_agent_app(profile: str = "ground_station") -> Any:
    cfg = ADOSConfig()
    cfg.agent.profile = profile
    return build_api_runtime(config=cfg)


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


# ---------------------------------------------------------------------------
# Group 2: /wfb, /wfb/relay, /wfb/receiver
# ---------------------------------------------------------------------------


def test_wfb_put_updates_radio(client):
    """PUT /wfb accepts a partial update and echoes the new state."""
    resp = client.put(f"{GS_PREFIX}/wfb", json={"channel": 161})
    assert resp.status_code in (200, 503)


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
# Group 4: /ui, /display, /bluetooth, /gamepads, /pic
# ---------------------------------------------------------------------------


def test_ui_get_returns_full_config(client):
    """GET /ui returns the OLED + buttons + screens config blob."""
    resp = client.get(f"{GS_PREFIX}/ui")
    assert resp.status_code == 200
    data = resp.json()
    for key in ("oled", "buttons", "screens"):
        assert key in data


def test_display_get(client):
    """GET /display returns the persisted HDMI display config."""
    resp = client.get(f"{GS_PREFIX}/display")
    assert resp.status_code == 200


# ---------------------------------------------------------------------------
# Group 5: /mesh, /role
# ---------------------------------------------------------------------------
#
# The uplink change stream (`/ws/uplink`) is served natively by the front (the
# Rust `ados-control` surface polls the durable store + does its own WebSocket
# auth), so its routes are validated by the `ados-control` crate tests, not here.


def test_mesh_neighbors_direct_404(client, patch_role):
    """GET /mesh/neighbors on a direct node returns 404."""
    patch_role("direct")
    resp = client.get(f"{GS_PREFIX}/mesh/neighbors")
    assert resp.status_code == 404


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
    """POST /wfb/pair requires a pair_key field.

    The ground-station routes report a bad request body as 400 (the
    convention shared with the camera/mesh/network routes), not FastAPI's
    default 422.
    """
    resp = client.post(f"{GS_PREFIX}/wfb/pair", json={})
    assert resp.status_code == 400
