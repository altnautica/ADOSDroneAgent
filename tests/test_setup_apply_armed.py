"""POST /api/v1/setup/apply refuses link-dropping sections while armed."""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

import ados.api.routes.setup.apply as apply_route
from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def restarts(monkeypatch) -> list[dict]:
    calls: list[dict] = []

    def fake_restart(data: dict) -> str:
        calls.append(data)
        return " Restart dispatched."

    monkeypatch.setattr(apply_route, "dispatch_profile_restart", fake_restart)
    return calls


@pytest.fixture
def agent_app():
    return build_api_runtime(state={"armed": True})


@pytest.fixture
def client(agent_app):
    return TestClient(create_app(agent_app))


_PROFILE = {"profile": "ground_station", "ground_role": "relay", "auto_restart": True}


def test_profile_with_auto_restart_is_refused_while_armed(client, agent_app, restarts) -> None:
    resp = client.post("/api/v1/setup/apply", json={"profile": _PROFILE})
    assert resp.status_code == 409
    assert resp.json()["detail"] == {
        "error": "E_ARMED",
        "message": "Vehicle is armed",
        "override": "force",
    }
    assert agent_app.config.agent.profile != "ground_station"
    assert restarts == []


def test_force_true_proceeds_while_armed(client, agent_app, restarts) -> None:
    resp = client.post("/api/v1/setup/apply", json={"profile": _PROFILE, "force": True})
    assert resp.status_code == 200
    assert resp.json()["sections"]["profile"]["ok"] is True
    assert agent_app.config.agent.profile == "ground_station"
    assert len(restarts) == 1


def test_ui_only_passes_while_armed(client) -> None:
    resp = client.post("/api/v1/setup/apply", json={"ui": {"theme": "light"}})
    assert resp.status_code == 200
    assert resp.json()["overall"] is True


def test_missing_state_counts_as_armed() -> None:
    client = TestClient(create_app(build_api_runtime()))
    resp = client.post("/api/v1/setup/apply", json={"wfb": {"channel": 161}})
    assert resp.status_code == 409
