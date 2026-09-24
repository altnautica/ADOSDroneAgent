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
# /display
# ---------------------------------------------------------------------------


def test_mesh_neighbors_direct_404(client, patch_role):
    """GET /mesh/neighbors on a direct node returns 404."""
    patch_role("direct")
    resp = client.get(f"{GS_PREFIX}/mesh/neighbors")
    assert resp.status_code == 404


# ---------------------------------------------------------------------------
# Group 6: /pair, /pairing
# ---------------------------------------------------------------------------


