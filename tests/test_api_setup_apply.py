"""Tests for POST /api/v1/setup/apply (batch settings apply)."""

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


def test_apply_empty_body_returns_ok_with_no_sections(client) -> None:
    resp = client.post("/api/v1/setup/apply", json={})
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is True
    assert data["sections"] == {}
    assert data["rolled_back"] == []


def test_apply_profile_only_applies_and_returns_ok(client, agent_app) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={"profile": {"profile": "ground_station", "ground_role": "relay"}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is True
    assert "profile" in data["sections"]
    assert data["sections"]["profile"]["ok"] is True
    assert agent_app.config.agent.profile == "ground_station"
    assert agent_app.config.ground_station.role == "relay"


def test_apply_profile_and_cloud_both_apply(client, agent_app) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={
            "profile": {"profile": "drone"},
            "cloud": {"mode": "local"},
        },
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is True
    assert data["sections"]["profile"]["ok"] is True
    assert data["sections"]["cloud"]["ok"] is True
    assert agent_app.config.agent.profile == "drone"
    assert agent_app.config.server.mode == "local"


def test_apply_regulatory_pins_region(client, agent_app) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={"regulatory": {"mode": "region", "region": "de"}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is True
    assert data["sections"]["regulatory"]["ok"] is True
    assert data["sections"]["regulatory"]["data"]["restart_required"] is True
    assert agent_app.config.network.regulatory.mode == "region"
    # Region code is uppercased on apply.
    assert agent_app.config.network.regulatory.region == "DE"
    assert agent_app.config.network.regulatory.ack_at is not None


def test_apply_regulatory_unrestricted_clears_region(client, agent_app) -> None:
    agent_app.config.network.regulatory.mode = "region"
    agent_app.config.network.regulatory.region = "DE"
    resp = client.post(
        "/api/v1/setup/apply",
        json={"regulatory": {"mode": "unrestricted"}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is True
    assert data["sections"]["regulatory"]["ok"] is True
    assert agent_app.config.network.regulatory.mode == "unrestricted"
    assert agent_app.config.network.regulatory.region is None


def test_apply_regulatory_bad_region_rejected(client, agent_app) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={"regulatory": {"mode": "region", "region": "USA"}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is False
    assert data["sections"]["regulatory"]["ok"] is False
    # Posture stays at the default; nothing was persisted.
    assert agent_app.config.network.regulatory.mode == "unrestricted"


def test_apply_advanced_bad_log_level_rolls_back_profile(client, agent_app) -> None:
    # Capture pre-state so we can assert rollback restored it.
    prior_profile = agent_app.config.agent.profile

    resp = client.post(
        "/api/v1/setup/apply",
        json={
            "profile": {"profile": "ground_station", "ground_role": "direct"},
            "advanced": {"log_level": "tachyon"},
        },
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is False
    assert data["sections"]["profile"]["ok"] is True
    assert data["sections"]["advanced"]["ok"] is False
    # Rollback list contains the section that succeeded then was reverted.
    assert "profile" in data["rolled_back"]
    # Live config is back to where it was before the call.
    assert agent_app.config.agent.profile == prior_profile


def test_apply_never_raises_500_on_bad_pydantic_input(client) -> None:
    # The profile field must be one of the literal values; an invalid
    # literal trips Pydantic and the route surfaces 422 (validation),
    # never an unhandled 500.
    resp = client.post(
        "/api/v1/setup/apply",
        json={"profile": {"profile": "definitely-not-a-valid-profile"}},
    )
    assert resp.status_code == 422
    body = resp.json()
    assert "detail" in body


def test_apply_unknown_top_level_field_is_ignored(client) -> None:
    # Pydantic's default behaviour is to ignore unknown fields, so a
    # caller that ships extra keys gets a clean ok response with no
    # sections processed.
    resp = client.post(
        "/api/v1/setup/apply",
        json={"banana": {"flavor": "yellow"}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is True
    assert data["sections"] == {}


def test_apply_cloud_self_hosted_missing_url_is_structured_failure(
    client,
) -> None:
    # Bad self-hosted payload (missing url) returns a structured
    # per-section failure, not a 5xx.
    resp = client.post(
        "/api/v1/setup/apply",
        json={"cloud": {"mode": "self_hosted"}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is False
    assert data["sections"]["cloud"]["ok"] is False


def test_apply_network_writes_to_live_config(client, agent_app) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={"network": {"wifi_ssid": "skynet", "hotspot_enabled": False}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is True
    assert data["sections"]["network"]["ok"] is True
    assert agent_app.config.network.wifi_client.ssid == "skynet"
    assert agent_app.config.network.hotspot.enabled is False


def test_apply_advanced_factory_reset_is_queued_only(client) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={"advanced": {"factory_reset": True}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is True
    section = data["sections"]["advanced"]
    assert section["ok"] is True
    assert section["data"].get("factory_reset_queued") is True


def test_apply_advanced_rejects_bad_board_override(client) -> None:
    resp = client.post(
        "/api/v1/setup/apply",
        json={"advanced": {"board_override": "../etc/passwd"}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is False
    assert data["sections"]["advanced"]["ok"] is False


def test_apply_ui_theme_writes_to_live_config(client, agent_app) -> None:
    # Default config theme is "dark"; flipping to "light" via /apply must
    # land on the live config and report through the section response.
    assert agent_app.config.ui.theme == "dark"
    resp = client.post(
        "/api/v1/setup/apply",
        json={"ui": {"theme": "light"}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is True
    section = data["sections"]["ui"]
    assert section["ok"] is True
    assert section["data"]["changed"] is True
    assert section["data"]["theme"] == "light"
    assert agent_app.config.ui.theme == "light"


def test_apply_ui_theme_rejects_unknown_value(client, agent_app) -> None:
    # Unknown literal trips Pydantic validation at the request boundary
    # and surfaces 422, not a 500.
    resp = client.post(
        "/api/v1/setup/apply",
        json={"ui": {"theme": "not-a-real-theme"}},
    )
    assert resp.status_code == 422
    body = resp.json()
    assert "detail" in body
    # Live config is untouched.
    assert agent_app.config.ui.theme == "dark"


def test_apply_ui_then_failing_advanced_rolls_back_theme(
    client, agent_app
) -> None:
    # Capture the prior theme so we can prove rollback restored it.
    prior_theme = agent_app.config.ui.theme
    assert prior_theme == "dark"
    resp = client.post(
        "/api/v1/setup/apply",
        json={
            "ui": {"theme": "light"},
            "advanced": {"log_level": "tachyon"},
        },
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is False
    assert data["sections"]["ui"]["ok"] is True
    assert data["sections"]["advanced"]["ok"] is False
    # The succeeded ui section must be reverted in reverse order.
    assert "ui" in data["rolled_back"]
    assert agent_app.config.ui.theme == prior_theme
