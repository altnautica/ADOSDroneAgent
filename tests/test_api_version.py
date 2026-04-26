"""Tests for /api/version capability negotiation endpoint.

Locks the response shape so any future change is forced to either
preserve it or bump api_version. Catches the kind of silent drift
that DEC-110 surfaced when /api/status/full landed without the GCS
knowing whether the agent supported it.
"""

from __future__ import annotations

import time
from unittest.mock import MagicMock

import pytest
from fastapi.testclient import TestClient

from ados import __version__
from ados.api.routes.version import API_VERSION, CAPABILITIES
from ados.api.server import create_app
from ados.core.config import ADOSConfig
from ados.core.health import HealthMonitor
from ados.core.main import ServiceTracker
from ados.services.mavlink.state import VehicleState


@pytest.fixture
def client():
    app = MagicMock()
    app.config = ADOSConfig()
    app.health = HealthMonitor()
    app.services = ServiceTracker()
    app._start_time = time.monotonic()
    app.uptime_seconds = 0.0
    app._vehicle_state = VehicleState()
    app._fc_connection = MagicMock()
    app._fc_connection.connected = False
    app._fc_connection.port = ""
    app._fc_connection.baud = 0
    app._tasks = []
    app._param_cache = None
    app.pairing_manager.is_paired = False
    return TestClient(create_app(app))


def test_version_endpoint_returns_expected_shape(client):
    resp = client.get("/api/version")
    assert resp.status_code == 200
    data = resp.json()
    assert set(data.keys()) == {"api_version", "agent_version", "capabilities"}


def test_api_version_is_string(client):
    resp = client.get("/api/version")
    data = resp.json()
    assert isinstance(data["api_version"], str)
    assert data["api_version"] == API_VERSION


def test_agent_version_matches_package_version(client):
    resp = client.get("/api/version")
    data = resp.json()
    assert data["agent_version"] == __version__


def test_capabilities_is_list_of_strings(client):
    resp = client.get("/api/version")
    data = resp.json()
    assert isinstance(data["capabilities"], list)
    for cap in data["capabilities"]:
        assert isinstance(cap, str)
        assert "." in cap, f"capability flag {cap!r} should be dot-namespaced"


def test_capabilities_includes_known_flags(client):
    resp = client.get("/api/version")
    data = resp.json()
    caps = set(data["capabilities"])
    # Sanity: at least the core endpoints we know shipped before today are present.
    expected = {
        "version.endpoint",
        "status.full",
        "services.control",
        "video.pipeline",
        "wfb.link",
        "scripts.runtime",
        "ota.updater",
        "pairing.mnemonic",
        "ground_station.profile",
        "ros.environment",
        "signing.mavlink",
    }
    missing = expected - caps
    assert not missing, f"missing expected capability flags: {missing}"


def test_capabilities_constant_is_unique():
    """No accidental duplicate flags in the canonical list."""
    assert len(set(CAPABILITIES)) == len(CAPABILITIES)
