"""Tests for `ados help`, `ados pair`, `ados unpair`, and the shared _ansi helpers."""

from __future__ import annotations

from unittest.mock import patch

from click.testing import CliRunner

from ados.cli import _ansi
from ados.cli.main import cli

runner = CliRunner()


# ── ados help ───────────────────────────────────────────────────────────────


def test_help_lists_the_primitive_commands() -> None:
    result = runner.invoke(cli, ["help"])
    assert result.exit_code == 0
    for cmd in ("ados status", "ados pair", "ados unpair", "ados uninstall", "ados update"):
        assert cmd in result.output
    assert "Advanced:" in result.output
    assert "rust" in result.output


# ── ados pair (info screen) ─────────────────────────────────────────────────


def _pair_status() -> dict:
    return {
        "device_name": "ados-a7z",
        "profile": "drone",
        "paired": False,
        "pairing_code": "7F3K9Q2M",
        "lan_host": "ados-a7z.local",
        "network": {"mdns_host": "ados-a7z.local", "api_port": 8080},
        "access_urls": [{"url": "http://192.168.1.50:8080/setup", "primary": True}],
    }


def test_pair_info_shows_host_code_and_radio() -> None:
    def fake_req(method: str, path: str, **_k):
        if path == "/api/v1/setup/status":
            return 200, _pair_status()
        if path == "/api/wfb/pair":
            return 200, {"paired": True, "role": "drone", "fingerprint": "42d004b0"}
        return 404, {}

    # The pair screen reads the setup facade off a systemd-managed agent; pin the
    # platform to Linux so the test exercises that path on any host (on macOS it
    # composes from the native /api/pairing/info route instead).
    with patch("ados.cli.pair.platform.system", return_value="Linux"), patch(
        "ados.cli.pair._req", side_effect=fake_req
    ):
        result = runner.invoke(cli, ["pair"])
    assert result.exit_code == 0
    assert "Connect to Mission Control" in result.output
    assert "ados-a7z.local" in result.output
    assert "192.168.1.50" in result.output
    assert "7F3K9Q2M" in result.output
    assert "Radio link" in result.output
    assert "127.0.0.1" not in result.output  # localhost is never shown as a reach host


def test_pair_role_triggers_local_bind() -> None:
    calls: list[tuple[str, str]] = []

    def fake_req(method: str, path: str, **_k):
        calls.append((method, path))
        if path == "/api/wfb/pair/local-bind":
            return 200, {"state": "paired", "fingerprint": "42d004b0"}
        return 200, {}

    with patch("ados.cli.pair._req", side_effect=fake_req):
        result = runner.invoke(cli, ["pair", "--role", "drone", "--yes"])
    assert result.exit_code == 0
    assert ("POST", "/api/wfb/pair/local-bind") in calls


# ── ados unpair ─────────────────────────────────────────────────────────────


def test_unpair_releases_node_only_by_default() -> None:
    calls: list[tuple[str, str]] = []

    def fake_req(method: str, path: str, **_k):
        calls.append((method, path))
        return 200, {}

    with patch("ados.cli.pair._req", side_effect=fake_req):
        result = runner.invoke(cli, ["unpair", "--yes"])
    assert result.exit_code == 0
    assert ("POST", "/api/pairing/unpair") in calls
    assert ("POST", "/api/wfb/pair/unpair") not in calls


def test_unpair_all_also_wipes_the_radio_bind() -> None:
    calls: list[tuple[str, str]] = []

    def fake_req(method: str, path: str, **_k):
        calls.append((method, path))
        return 200, {"role": "drone"}

    with patch("ados.cli.pair._req", side_effect=fake_req):
        result = runner.invoke(cli, ["unpair", "--all", "--yes"])
    assert result.exit_code == 0
    assert ("POST", "/api/pairing/unpair") in calls
    assert ("POST", "/api/wfb/pair/unpair") in calls


# ── _ansi reach-URL helpers ─────────────────────────────────────────────────


def test_order_reach_urls_puts_mdns_first_localhost_last() -> None:
    urls = [
        "http://127.0.0.1:8080",
        "http://192.168.1.5:8080",
        "http://ados-a7z.local:8080",
    ]
    ordered = _ansi.order_reach_urls(urls)
    assert ordered[0] == "http://ados-a7z.local:8080"
    assert ordered[-1] == "http://127.0.0.1:8080"


def test_reach_block_drops_localhost_when_a_lan_address_exists() -> None:
    theme = _ansi.Theme(color=False, ascii=True)
    lines = _ansi.reach_block(theme, ["http://127.0.0.1:8080", "http://host.local:8080"])
    body = "\n".join(lines)
    assert "host.local:8080" in body
    assert "127.0.0.1" not in body


def test_reach_block_shows_localhost_only_as_last_resort() -> None:
    theme = _ansi.Theme(color=False, ascii=True)
    lines = _ansi.reach_block(theme, ["http://127.0.0.1:8080"])
    body = "\n".join(lines)
    assert "127.0.0.1:8080" in body
    assert "on-box only" in body


def test_run_steps_plain_records_results_and_continues_past_failure() -> None:
    theme = _ansi.Theme(color=False, ascii=True)

    def _boom() -> str:
        raise RuntimeError("kaboom")

    results = _ansi.run_steps(
        theme,
        [("ok step", lambda: "detail"), ("bad step", _boom), ("after", lambda: None)],
        title="Test",
        interactive=False,
    )
    assert [r.ok for r in results] == [True, False, True]
    assert results[1].detail == "kaboom"
