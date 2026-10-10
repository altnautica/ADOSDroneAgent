"""Tests for the WFB section of POST /api/v1/setup/apply."""

from __future__ import annotations

import json

import httpx
import pytest
from fastapi.testclient import TestClient

import ados.setup.profile as profile_mod
from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


class _NativeFront:
    """A stand-in for the native control surface's WFB routes."""

    def __init__(self, answers: dict[tuple[str, str], tuple[int, dict]]) -> None:
        self.answers = answers
        self.seen: list[tuple[str, str, dict]] = []

    def __call__(self, request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content or b"{}")
        self.seen.append((request.method, request.url.path, body))
        status, answer = self.answers[(request.method, request.url.path)]
        return httpx.Response(status, json=answer)


@pytest.fixture
def native(monkeypatch) -> _NativeFront:
    front = _NativeFront(
        {
            ("POST", "/api/wfb/channel"): (
                202,
                {"status": "initiated", "target_channel": 161, "frequency_mhz": 5805},
            ),
            ("PUT", "/api/wfb/tx-power"): (
                200,
                {"requested_dbm": 8, "effective_dbm": 8, "persisted": True},
            ),
        }
    )
    monkeypatch.setattr(profile_mod, "_control_transport", lambda: httpx.MockTransport(front))
    return front


@pytest.fixture
def agent_app():
    return build_api_runtime(state={"armed": False})


@pytest.fixture
def client(agent_app):
    return TestClient(create_app(agent_app))


def test_apply_wfb_uses_native_routes(client, native) -> None:
    resp = client.post("/api/v1/setup/apply", json={"wfb": {"channel": 161, "tx_power_dbm": 8}})
    assert resp.status_code == 200
    section = resp.json()["sections"]["wfb"]
    assert section["ok"] is True
    assert [(m, p) for m, p, _ in native.seen] == [
        ("POST", "/api/wfb/channel"),
        ("PUT", "/api/wfb/tx-power"),
    ]
    assert native.seen[0][2] == {"channel": 161, "force": False}
    assert native.seen[1][2] == {"tx_power_dbm": 8, "force": False}
    assert section["data"]["native"]["channel"]["status"] == 202
    assert section["data"]["native"]["tx_power_dbm"]["body"]["persisted"] is True
    # A live hop needs no restart.
    assert "restart_required" not in section["data"]


def test_apply_wfb_forwards_the_armed_override(native) -> None:
    from ados.setup.models import WfbApplyRequest

    app = build_api_runtime(state={"armed": False})
    result = profile_mod.apply_wfb(app, WfbApplyRequest(channel=161), force=True)
    assert result.ok is True
    assert native.seen[0][2] == {"channel": 161, "force": True}


def test_apply_wfb_reports_a_native_refusal(client, native) -> None:
    native.answers[("POST", "/api/wfb/channel")] = (
        400,
        {"detail": "Invalid channel 7. Valid channels: [36]"},
    )
    resp = client.post("/api/v1/setup/apply", json={"wfb": {"channel": 7}})
    section = resp.json()["sections"]["wfb"]
    assert section["ok"] is False
    assert "400" in section["message"]
    assert section["data"]["native"]["channel"]["status"] == 400


def test_apply_wfb_unchanged_channel_makes_no_call(client, agent_app, native) -> None:
    current = agent_app.config.video.wfb.channel
    resp = client.post("/api/v1/setup/apply", json={"wfb": {"channel": current}})
    assert resp.json()["sections"]["wfb"]["ok"] is True
    assert native.seen == []


def test_apply_wfb_mcs_index_persists_and_marks_reboot(client, agent_app, native) -> None:
    resp = client.post("/api/v1/setup/apply", json={"wfb": {"mcs_index": 4}})
    data = resp.json()
    assert data["sections"]["wfb"]["ok"] is True
    assert agent_app.config.video.wfb.mcs_index == 4
    assert data["sections"]["wfb"]["data"].get("restart_required") is True
    assert native.seen == []


def test_apply_wfb_invalid_mcs_makes_no_native_call(client, native) -> None:
    resp = client.post("/api/v1/setup/apply", json={"wfb": {"mcs_index": 9, "channel": 161}})
    assert resp.json()["sections"]["wfb"]["ok"] is False
    assert native.seen == []


def test_apply_wfb_topology_persists(client, agent_app, native) -> None:
    resp = client.post("/api/v1/setup/apply", json={"wfb": {"topology": "powered_hub"}})
    assert resp.json()["sections"]["wfb"]["ok"] is True
    assert agent_app.config.video.wfb.topology == "powered_hub"


def test_apply_wfb_rollback_when_advanced_fails(client, agent_app, native) -> None:
    """Rollback restores the wfb config leaves when a later section blows up."""
    prior_mcs = agent_app.config.video.wfb.mcs_index
    new_mcs = 5 if prior_mcs != 5 else 6
    resp = client.post(
        "/api/v1/setup/apply",
        json={"wfb": {"mcs_index": new_mcs}, "advanced": {"log_level": "tachyon"}},
    )
    data = resp.json()
    assert data["overall"] is False
    assert data["sections"]["wfb"]["ok"] is True
    assert data["sections"]["advanced"]["ok"] is False
    assert "wfb" in data["rolled_back"]
    assert agent_app.config.video.wfb.mcs_index == prior_mcs
