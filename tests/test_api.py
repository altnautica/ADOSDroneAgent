"""Tests for FastAPI REST API routes."""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def agent_app():
    """Create an API runtime double for testing."""
    return build_api_runtime()


@pytest.fixture
def client(agent_app):
    """FastAPI test client."""
    fastapi_app = create_app(agent_app)
    return TestClient(fastapi_app)


def test_get_setup_status(client):
    resp = client.get("/api/v1/setup/status")
    assert resp.status_code == 200
    data = resp.json()
    assert data["device_id"]
    assert "steps" in data
    assert "access_urls" in data
    assert "mavlink" in data
    assert "video" in data
    # A live agent always reports "configured". Auto-detect commits a
    # profile at install, the operator can override via the webapp at
    # any time, and there is no intermediate "needs review" state.
    assert data["setup_state"] == "configured"
    assert data["profile_source"] in (
        "detected",
        "tiebreaker",
        "override",
        "default",
        "user",
    )
    assert data["profile_suggestion"]["detected"] in ("drone", "ground_station")
    assert data["profile_suggestion"]["source"] in (
        "detected",
        "tiebreaker",
        "override",
        "default",
    )


def test_get_config(client):
    resp = client.get("/api/config")
    assert resp.status_code == 200
    data = resp.json()
    assert "agent" in data
    assert "mavlink" in data


def test_update_config(client):
    resp = client.put("/api/config", json={"key": "agent.name", "value": "new-drone"})
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "ok"


def test_get_logs_degrades_when_store_unreachable(client):
    """With no logging store socket present (the test environment), the legacy
    /api/logs endpoint degrades to an empty list with a warning rather than a
    500: losing history degrades debugging, not flight."""
    resp = client.get("/api/logs")
    assert resp.status_code == 200
    data = resp.json()
    assert data["entries"] == []
    assert data["total"] == 0
    assert "warning" in data


def test_legacy_entry_timestamp_is_iso_string():
    """The legacy entry mapping must emit an ISO-8601 string timestamp, not a
    float. A numeric timestamp breaks consumers that slice/parse the field as a
    string (the live dashboard log view does exactly that)."""
    from datetime import datetime

    from ados.api.routes.logs import _legacy_entry

    entry = _legacy_entry(
        {
            "id": 7,
            "ts_us": 1_700_000_000_000_000,
            "source": "python-agent",
            "level": "info",
            "target": "ados.test",
            "msg": "hello",
        }
    )
    assert isinstance(entry["timestamp"], str)
    # Round-trips through the ISO parser without raising.
    datetime.fromisoformat(entry["timestamp"])
    # The legacy consumer expects an upper-case level and the logger name.
    assert entry["level"] == "INFO"
    assert entry["logger"] == "ados.test"
    assert entry["message"] == "hello"


