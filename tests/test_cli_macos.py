"""macOS workstation CLI behavior.

The macOS node is a rootless, Rust-only workstation under launchd. These tests
pin the platform so they run deterministically on any host and cover the
macOS-specific behaviors: reading status from the native control-surface routes
(the proxied setup facade is absent) and the real launchd teardown on uninstall.
"""

from __future__ import annotations

import subprocess

from click.testing import CliRunner

import ados.cli._main_status as cli_status
import ados.cli._main_uninstall as cli_uninstall
import ados.cli.main as cli_main


def test_native_status_composes_from_native_routes():
    """`_setup_status` on macOS builds the display dict from the native routes,
    never the absent proxied `/api/v1/setup/status`, and invents nothing."""
    from ados.cli._agent_http import AgentResponse

    answers = {
        "/api/status/full": {"version": "1.2.3", "profile": "workstation", "fc_connected": False},
        "/api/pairing/info": {
            "name": "my-mac",
            "profile": "workstation",
            "paired": True,
            "pairing_code": None,
            "mdns_host": "ados-abcd12.local",
        },
        "/api/config": {"server": {"mode": "local"}},
    }

    def fake_call(method: str, path: str, **_k):
        if path not in answers:
            raise AssertionError(f"unexpected path {path}")
        return AgentResponse(200, answers[path], "http://127.0.0.1:8082")

    from unittest.mock import patch

    with patch.object(cli_status, "IS_MACOS", True), patch.object(
        cli_status._agent_http, "call", side_effect=fake_call
    ):
        data = cli_main._setup_status()

    assert data["device_name"] == "my-mac"
    assert data["profile"] == "workstation"
    assert data["version"] == "1.2.3"
    assert data["paired"] is True
    assert data["lan_host"] == "ados-abcd12.local"
    assert data["cloud_choice"]["mode"] == "local"
    assert data["network"]["api_port"] == 8082
    assert data["completion_percent"] == "unknown"


def test_uninstall_macos_boots_out_agents_and_purges(tmp_path, monkeypatch):
    """`ados uninstall --purge` on macOS boots out every LaunchAgent, removes the
    plists, and deletes `~/.ados`."""
    home = tmp_path
    ados_home = home / ".ados"
    (ados_home / "bin").mkdir(parents=True)
    launch_agents = home / "Library" / "LaunchAgents"
    launch_agents.mkdir(parents=True)
    for tail in ("supervisor", "control", "cloud", "logd"):
        (launch_agents / f"co.ados.{tail}.plist").write_text("x")

    monkeypatch.setattr(cli_uninstall.platform, "system", lambda: "Darwin")
    monkeypatch.setattr(cli_uninstall.Path, "home", staticmethod(lambda: home))
    monkeypatch.setenv("ADOS_HOME", str(ados_home))
    monkeypatch.setattr(cli_uninstall.os, "getuid", lambda: 501)

    booted: list[str] = []

    def fake_run(cmd, **_k):
        if cmd[:2] == ["launchctl", "print"]:
            return subprocess.CompletedProcess(cmd, 0, "", "")
        if cmd[:2] == ["launchctl", "bootout"]:
            booted.append(cmd[2])
            return subprocess.CompletedProcess(cmd, 0, "", "")
        # pip/pipx/uv probes: pretend the CLI is not package-managed.
        return subprocess.CompletedProcess(cmd, 1, "", "not found")

    monkeypatch.setattr(cli_uninstall.subprocess, "run", fake_run)

    res = CliRunner().invoke(cli_main.cli, ["uninstall", "--purge", "--yes"])
    assert res.exit_code == 0, res.output
    # All four daemons booted out of the user's GUI domain.
    assert len(booted) == 4
    assert booted == [f"gui/501/co.ados.{t}" for t in ("supervisor", "control", "cloud", "logd")]
    # Plists removed, and --purge dropped ~/.ados entirely.
    assert not list(launch_agents.glob("co.ados.*.plist"))
    assert not ados_home.exists()


def test_uninstall_macos_without_purge_keeps_home(tmp_path, monkeypatch):
    """Without --purge, the teardown boots out the agents but preserves ~/.ados."""
    home = tmp_path
    ados_home = home / ".ados"
    (ados_home / "bin").mkdir(parents=True)
    launch_agents = home / "Library" / "LaunchAgents"
    launch_agents.mkdir(parents=True)
    (launch_agents / "co.ados.control.plist").write_text("x")

    monkeypatch.setattr(cli_uninstall.platform, "system", lambda: "Darwin")
    monkeypatch.setattr(cli_uninstall.Path, "home", staticmethod(lambda: home))
    monkeypatch.setenv("ADOS_HOME", str(ados_home))
    monkeypatch.setattr(cli_uninstall.os, "getuid", lambda: 501)

    def fake_run(cmd, **_k):
        if cmd[:2] == ["launchctl", "print"]:
            return subprocess.CompletedProcess(cmd, 0, "", "")
        return subprocess.CompletedProcess(cmd, 1, "", "")

    monkeypatch.setattr(cli_uninstall.subprocess, "run", fake_run)

    res = CliRunner().invoke(cli_main.cli, ["uninstall", "--yes"])
    assert res.exit_code == 0, res.output
    assert not list(launch_agents.glob("co.ados.*.plist"))
    assert ados_home.exists()  # identity + config preserved for a re-install
