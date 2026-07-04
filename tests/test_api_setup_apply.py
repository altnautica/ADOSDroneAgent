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


def test_apply_advanced_log_level_persists_to_logging_config(
    client, agent_app
) -> None:
    # The log level lands on config.logging.level (the field every service
    # reads at start), not a phantom agent.log_level, and is reported changed.
    assert agent_app.config.logging.level != "debug"
    resp = client.post(
        "/api/v1/setup/apply",
        json={"advanced": {"log_level": "debug"}},
    )
    assert resp.status_code == 200
    data = resp.json()
    assert data["overall"] is True
    section = data["sections"]["advanced"]
    assert section["ok"] is True
    assert "log_level" in section["data"]["fields"]
    assert agent_app.config.logging.level == "debug"


def test_apply_advanced_board_override_writes_file(
    client, monkeypatch, tmp_path
) -> None:
    # A valid slug is written to the board_override file the HAL detector
    # reads (honoring the ADOS_ETC_DIR sandbox override), not silently
    # acknowledged. Clearing it removes the file.
    monkeypatch.setenv("ADOS_ETC_DIR", str(tmp_path))
    from ados.setup.advanced import board_override_path, read_board_override

    resp = client.post(
        "/api/v1/setup/apply",
        json={"advanced": {"board_override": "rpi4b"}},
    )
    assert resp.status_code == 200
    assert resp.json()["sections"]["advanced"]["ok"] is True
    assert board_override_path().exists()
    assert read_board_override() == "rpi4b"

    # Clearing writes nothing back and removes the file.
    resp = client.post(
        "/api/v1/setup/apply",
        json={"advanced": {"board_override": ""}},
    )
    assert resp.status_code == 200
    assert resp.json()["sections"]["advanced"]["ok"] is True
    assert not board_override_path().exists()
    assert read_board_override() == ""


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


class _FailingSaveRuntime:
    """Minimal runtime whose save_config reports the given persist outcome.

    Lets the setter unit tests exercise the loud-fail path: a save that
    returns False (or raises) must surface ok=False, never a phantom
    success.
    """

    def __init__(self, config, *, save_result=True, raise_on_save=False) -> None:
        self.config = config
        self._save_result = save_result
        self._raise_on_save = raise_on_save
        self.raw_runtime = self

    def save_config(self):
        if self._raise_on_save:
            raise OSError("disk full")
        return self._save_result


def test_apply_network_surfaces_a_failed_persist() -> None:
    from ados.core.config import ADOSConfig
    from ados.setup.models import NetworkApplyRequest
    from ados.setup.network import apply_network

    req = NetworkApplyRequest(hotspot_enabled=True)

    # A save that returns falsy must surface ok=False, not a phantom success.
    runtime = _FailingSaveRuntime(ADOSConfig(), save_result=False)
    result = apply_network(runtime, req)
    assert result.ok is False
    assert "not saved" in result.message.lower()

    # A save that raises must also surface ok=False.
    runtime = _FailingSaveRuntime(ADOSConfig(), raise_on_save=True)
    result = apply_network(runtime, NetworkApplyRequest(hotspot_enabled=True))
    assert result.ok is False

    # A successful save reports ok=True and the persisted change.
    runtime = _FailingSaveRuntime(ADOSConfig(), save_result=True)
    result = apply_network(runtime, NetworkApplyRequest(hotspot_enabled=True))
    assert result.ok is True
    assert "hotspot_enabled" in result.data["fields"]


def test_apply_advanced_surfaces_a_failed_log_level_persist() -> None:
    from ados.core.config import ADOSConfig
    from ados.setup.advanced import apply_advanced
    from ados.setup.models import AdvancedApplyRequest

    runtime = _FailingSaveRuntime(ADOSConfig(), save_result=False)
    result = apply_advanced(runtime, AdvancedApplyRequest(log_level="debug"))
    assert result.ok is False
    assert "not saved" in result.message.lower()
