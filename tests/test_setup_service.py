"""Tests for universal setup helpers."""

from __future__ import annotations

import pytest

from ados.setup.service import (
    _build_known_hosts,
    _mission_control_url,
    _safe_host_for,
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
