"""Tests for the WFB section of POST /api/v1/setup/apply."""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def agent_app():
    return build_api_runtime()


@pytest.fixture
def client(agent_app):
    fastapi_app = create_app(agent_app)
    return TestClient(fastapi_app)


def test_apply_wfb_channel_persists_to_config(client, agent_app) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={"wfb": {"channel": 161}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is True
    assert data["sections"]["wfb"]["ok"] is True
    assert agent_app.config.video.wfb.channel == 161
    # Channel changes are reboot-required.
    assert data["sections"]["wfb"]["data"].get("restart_required") is True


def test_apply_wfb_invalid_channel_rejects(client, agent_app) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={"wfb": {"channel": 7}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["sections"]["wfb"]["ok"] is False


def test_apply_wfb_tx_power_persists(client, agent_app) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={"wfb": {"tx_power_dbm": 8}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["sections"]["wfb"]["ok"] is True
    assert agent_app.config.video.wfb.tx_power_dbm == 8


def test_apply_wfb_tx_power_above_ceiling_rejects(client, agent_app) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={"wfb": {"tx_power_dbm": 99}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["sections"]["wfb"]["ok"] is False


def test_apply_wfb_mcs_index_persists_and_marks_reboot(client, agent_app) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={"wfb": {"mcs_index": 4}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["sections"]["wfb"]["ok"] is True
    assert agent_app.config.video.wfb.mcs_index == 4
    assert data["sections"]["wfb"]["data"].get("restart_required") is True


def test_apply_wfb_topology_persists(client, agent_app) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={"wfb": {"topology": "powered_hub"}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["sections"]["wfb"]["ok"] is True
    assert agent_app.config.video.wfb.topology == "powered_hub"


def test_apply_wfb_rollback_when_advanced_fails(client, agent_app) -> None:
    """Rollback should restore wfb config when a later section blows up."""
    prior_channel = agent_app.config.video.wfb.channel
    resp = client.post(
        "/api/v1/setup/apply",
        json={
            "wfb": {"channel": 165},
            "advanced": {"log_level": "tachyon"},
        },
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is False
    assert data["sections"]["wfb"]["ok"] is True
    assert data["sections"]["advanced"]["ok"] is False
    assert "wfb" in data["rolled_back"]
    assert agent_app.config.video.wfb.channel == prior_channel
