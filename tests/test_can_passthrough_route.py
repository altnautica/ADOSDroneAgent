"""Tests for /api/can/passthrough stub.

The route is reserved for a future agent-side CAN bridge. Today it
returns HTTP 501 with a structured JSON envelope so the GCS can probe
for availability without crashing on a 404 and can distinguish a
deliberate not-implemented response from a missing surface.
"""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def client():
    app = build_api_runtime(uptime_seconds=0.0)
    return TestClient(create_app(app))


def test_can_passthrough_returns_501(client):
    resp = client.post("/api/can/passthrough")
    assert resp.status_code == 501


def test_can_passthrough_body_shape(client):
    resp = client.post("/api/can/passthrough")
    data = resp.json()
    assert data == {
        "error": "not_implemented",
        "message": "CAN passthrough planned for future agent-side support",
    }


def test_can_passthrough_appears_in_openapi(client):
    resp = client.get("/openapi.json")
    assert resp.status_code == 200
    schema = resp.json()
    paths = schema.get("paths", {})
    assert "/api/can/passthrough" in paths
    assert "post" in paths["/api/can/passthrough"]
