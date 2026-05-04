"""Tests for universal setup helpers."""

from __future__ import annotations

import pytest

from ados.setup.service import (
    _build_known_hosts,
    _cloud_choice_status,
    _mission_control_url,
    _safe_host_for,
    apply_cloud_choice,
    extract_cloudflare_token,
)


def test_extract_cloudflare_token_from_raw_token() -> None:
    assert extract_cloudflare_token("a" * 40) == "a" * 40


def test_extract_cloudflare_token_from_install_command() -> None:
    command = "sudo cloudflared service install abcdefghijklmnopqrstuvwxyz123456"
    assert extract_cloudflare_token(command) == "abcdefghijklmnopqrstuvwxyz123456"


def test_extract_cloudflare_token_rejects_missing_token() -> None:
    with pytest.raises(ValueError):
        extract_cloudflare_token("sudo cloudflared service install")


# --- host-header validation ---------------------------------------------------


class _CfRemote:
    setup_url = ""
    api_url = ""


class _Cf:
    cloudflare = _CfRemote()


class _Cfg:
    remote_access = _Cf()


def test_safe_host_for_accepts_known_host() -> None:
    hosts = _build_known_hosts(
        local_ips=["10.0.0.5"], mdns_host="ados-test.local", config=_Cfg()
    )
    assert "10.0.0.5" in hosts
    assert "localhost" in hosts
    assert "192.168.4.1" in hosts  # hotspot
    assert "192.168.7.1" in hosts  # USB gadget
    assert "ados-test.local" in hosts
    assert _safe_host_for("10.0.0.5:8080", hosts) == "10.0.0.5:8080"
    assert _safe_host_for("ados-test.local:8080", hosts) == "ados-test.local:8080"


def test_safe_host_for_rejects_unknown_host() -> None:
    hosts = _build_known_hosts(local_ips=[], mdns_host="ados.local", config=_Cfg())
    assert _safe_host_for("attacker.example.com", hosts) == "localhost:8080"
    # Empty / missing falls back to localhost too.
    assert _safe_host_for("", hosts) == "localhost:8080"
    assert _safe_host_for(None, hosts) == "localhost:8080"


def test_safe_host_for_first_value_wins_in_chain() -> None:
    hosts = _build_known_hosts(local_ips=["10.0.0.5"], mdns_host="", config=_Cfg())
    assert _safe_host_for("10.0.0.5:8080, attacker.example.com", hosts) == "10.0.0.5:8080"


# --- Mission Control URL omission --------------------------------------------


class _Scripting:
    mission_control_url = ""


class _MCCfg:
    scripting = _Scripting()


def test_mission_control_url_returns_localhost_when_caller_is_localhost() -> None:
    assert _mission_control_url(host_name="localhost", config=_MCCfg()) == "http://localhost:4000"
    assert _mission_control_url(host_name="127.0.0.1", config=_MCCfg()) == "http://localhost:4000"


def test_mission_control_url_is_empty_for_remote_callers() -> None:
    # Phone on the hotspot / LAN: no useful localhost link.
    assert _mission_control_url(host_name="192.168.4.1", config=_MCCfg()) == ""
    assert _mission_control_url(host_name="10.0.0.5", config=_MCCfg()) == ""


def test_mission_control_url_honours_explicit_config() -> None:
    cfg = _MCCfg()
    cfg.scripting.mission_control_url = "https://command.altnautica.com"
    assert (
        _mission_control_url(host_name="10.0.0.5", config=cfg)
        == "https://command.altnautica.com"
    )


# --- cloud-choice -------------------------------------------------------------


class _Cloud:
    url = "https://convex-site.altnautica.com"
    mqtt_broker = "mqtt.altnautica.com"
    mqtt_port = 443


class _SelfHosted:
    url = ""
    mqtt_broker = ""
    mqtt_port = 8883
    api_key = ""


class _Server:
    mode = "cloud"
    cloud = _Cloud()
    self_hosted = _SelfHosted()
    mqtt_password = ""


class _CloudChoiceConfig:
    server = _Server()


def test_cloud_choice_status_local_returns_minimal_record() -> None:
    cfg = _CloudChoiceConfig()
    cfg.server.mode = "local"
    cs = _cloud_choice_status(cfg)
    assert cs.mode == "local"
    assert cs.paired is False
    assert cs.pair_code_required is False
    assert cs.backend_url == ""


def test_cloud_choice_status_cloud_carries_backend_url() -> None:
    cfg = _CloudChoiceConfig()
    cfg.server.mode = "cloud"
    cs = _cloud_choice_status(cfg)
    assert cs.mode == "cloud"
    assert cs.pair_code_required is True
    assert cs.backend_url == "https://convex-site.altnautica.com"


def test_cloud_choice_status_self_hosted_reports_url() -> None:
    cfg = _CloudChoiceConfig()
    cfg.server.mode = "self_hosted"
    cfg.server.self_hosted.url = "https://convex.example.com"
    cs = _cloud_choice_status(cfg)
    assert cs.mode == "self_hosted"
    assert cs.backend_url == "https://convex.example.com"


class _RawRuntime:
    def save_config(self) -> None:
        pass


class _Runtime:
    def __init__(self) -> None:
        self.config = _CloudChoiceConfig()
        self.raw_runtime = _RawRuntime()


def test_apply_cloud_choice_local_clears_mqtt_password() -> None:
    runtime = _Runtime()
    runtime.config.server.mqtt_password = "leftover"
    result = apply_cloud_choice(runtime, mode="local")
    assert result.ok is True
    assert runtime.config.server.mode == "local"
    assert runtime.config.server.mqtt_password == ""


def test_apply_cloud_choice_self_hosted_persists_url_and_port() -> None:
    runtime = _Runtime()
    result = apply_cloud_choice(
        runtime,
        mode="self_hosted",
        self_hosted={
            "url": "https://convex.example.com",
            "mqtt_broker": "mqtt.example.com",
            "mqtt_port": 8884,
        },
    )
    assert result.ok is True
    assert runtime.config.server.mode == "self_hosted"
    assert runtime.config.server.self_hosted.url == "https://convex.example.com"
    assert runtime.config.server.self_hosted.mqtt_broker == "mqtt.example.com"
    assert runtime.config.server.self_hosted.mqtt_port == 8884


def test_apply_cloud_choice_self_hosted_requires_url() -> None:
    runtime = _Runtime()
    result = apply_cloud_choice(
        runtime,
        mode="self_hosted",
        self_hosted={"url": ""},
    )
    assert result.ok is False


def test_apply_cloud_choice_rejects_self_hosted_block_in_other_modes() -> None:
    runtime = _Runtime()
    result = apply_cloud_choice(
        runtime,
        mode="cloud",
        self_hosted={"url": "https://example.com"},
    )
    assert result.ok is False


def test_apply_cloud_choice_rejects_unknown_mode() -> None:
    runtime = _Runtime()
    result = apply_cloud_choice(runtime, mode="quantum")
    assert result.ok is False


def test_apply_cloud_choice_validates_port_range() -> None:
    runtime = _Runtime()
    result = apply_cloud_choice(
        runtime,
        mode="self_hosted",
        self_hosted={"url": "https://example.com", "mqtt_port": 99999},
    )
    assert result.ok is False


# --- AP defaults --------------------------------------------------------------


def test_hotspot_default_password_is_altnautica() -> None:
    """The agent's out-of-the-box AP passphrase is a known default so an
    operator can connect from a phone at the bench without reading a
    generated value off disk. Operators who need a unique passphrase set
    `network.hotspot.password` in /etc/ados/config.yaml.
    """
    from ados.core.config import HotspotConfig

    assert HotspotConfig().password == "altnautica"


# --- setup state (gate) ------------------------------------------------------


from pathlib import Path  # noqa: E402

from ados.setup import state as setup_state  # noqa: E402


def _redirect_state_path(monkeypatch, tmp_path: Path) -> Path:
    target = tmp_path / "state.json"
    monkeypatch.setattr(setup_state, "SETUP_STATE_PATH", target)
    monkeypatch.setattr(setup_state, "SETUP_STATE_DIR", tmp_path)
    return target


def test_setup_state_defaults_when_file_missing(monkeypatch, tmp_path) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    s = setup_state.read_state()
    assert s.setup_finalized is False
    assert s.skipped_steps == set()


def test_setup_state_mark_finalized_persists(monkeypatch, tmp_path) -> None:
    target = _redirect_state_path(monkeypatch, tmp_path)
    setup_state.mark_finalized()
    assert target.exists()
    s2 = setup_state.read_state()
    assert s2.setup_finalized is True


def test_setup_state_mark_and_clear_skip(monkeypatch, tmp_path) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    setup_state.mark_skipped("video")
    setup_state.mark_skipped("remote_access")
    s = setup_state.read_state()
    assert s.skipped_steps == {"video", "remote_access"}
    setup_state.clear_skipped("video")
    s2 = setup_state.read_state()
    assert s2.skipped_steps == {"remote_access"}


def test_setup_state_reset_clears_everything(monkeypatch, tmp_path) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    setup_state.mark_finalized()
    setup_state.mark_skipped("mavlink")
    setup_state.reset_state()
    s = setup_state.read_state()
    assert s.setup_finalized is False
    assert s.skipped_steps == set()


def test_setup_state_corrupt_file_returns_defaults(monkeypatch, tmp_path) -> None:
    target = _redirect_state_path(monkeypatch, tmp_path)
    target.write_text("not valid json {{{", encoding="utf-8")
    s = setup_state.read_state()
    assert s.setup_finalized is False
    assert s.skipped_steps == set()
