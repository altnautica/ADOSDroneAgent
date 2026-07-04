"""macOS workstation CLI behavior.

The macOS node is a rootless, Rust-only workstation under launchd. These tests
pin the platform so they run deterministically on any host and cover the three
macOS-specific behaviors: reading status from the native control-surface routes
(the proxied setup facade is absent), the honest no-op of the native-vs-packaged
`ados rust` toggle, and the real launchd teardown on uninstall.
"""

from __future__ import annotations

import subprocess

from click.testing import CliRunner

import ados.cli.main as cli_main
import ados.cli.rust as rust_mod
from ados.cli.rust import rust_group


def test_native_status_composes_from_native_routes():
    """`_setup_status` on macOS builds the display dict from `/api/status` +
    `/api/pairing/info`, never the absent proxied `/api/v1/setup/status`."""

    def fake_request(method: str, path: str, **_k):
        if path == "/api/status":
            return {"version": "1.2.3", "fc_connected": False, "fc_port": ""}
        if path == "/api/pairing/info":
            return {
                "name": "my-mac",
                "profile": "workstation",
                "paired": True,
                "pairing_code": None,
                "mdns_host": "ados-abcd12.local",
            }
        raise AssertionError(f"unexpected path {path}")

    from unittest.mock import patch

    with patch.object(cli_main, "IS_MACOS", True), patch.object(
        cli_main, "_request", side_effect=fake_request
    ):
        data = cli_main._setup_status()

    assert data["device_name"] == "my-mac"
    assert data["profile"] == "workstation"
    assert data["version"] == "1.2.3"
    assert data["paired"] is True
    assert data["lan_host"] == "ados-abcd12.local"
    assert data["cloud_choice"]["mode"] == "local"
    assert data["completion_percent"] == 100


def test_rust_toggle_is_a_clear_noop_on_macos(monkeypatch):
    """`ados rust status/enable/disable` on macOS says not-applicable and does
    NOT write a marker, drive systemctl, or claim a false success."""
    monkeypatch.setattr(rust_mod.platform, "system", lambda: "Darwin")
    touched: list[str] = []
    monkeypatch.setattr(rust_mod, "_apply", lambda *a, **k: touched.append("apply"))
    monkeypatch.setattr(
        rust_mod, "_systemctl", lambda *a, **k: touched.append("systemctl") or 0
    )

    for cmd in (["status"], ["enable", "control"], ["disable", "control"]):
        res = CliRunner().invoke(rust_group, cmd)
        assert res.exit_code == 0, res.output
        assert "not applicable on macOS" in res.output
        assert "enabled" not in res.output  # never a no-op success claim
    assert touched == []  # nothing was actually toggled


def test_uninstall_macos_boots_out_agents_and_purges(tmp_path, monkeypatch):
    """`ados uninstall --purge` on macOS boots out every LaunchAgent, removes the
    plists, and deletes `~/.ados`."""
    home = tmp_path
    ados_home = home / ".ados"
    (ados_home / "bin").mkdir(parents=True)
    launch_agents = home / "Library" / "LaunchAgents"
    launch_agents.mkdir(parents=True)
    for tail in ("supervisor", "control", "compute", "cloud", "logd"):
        (launch_agents / f"co.ados.{tail}.plist").write_text("x")

    monkeypatch.setattr(cli_main.platform, "system", lambda: "Darwin")
    monkeypatch.setattr(cli_main.Path, "home", staticmethod(lambda: home))
    monkeypatch.setenv("ADOS_HOME", str(ados_home))
    monkeypatch.setattr(cli_main.os, "getuid", lambda: 501)

    booted: list[str] = []

    def fake_run(cmd, **_k):
        if cmd[:2] == ["launchctl", "print"]:
            return subprocess.CompletedProcess(cmd, 0, "", "")
        if cmd[:2] == ["launchctl", "bootout"]:
            booted.append(cmd[2])
            return subprocess.CompletedProcess(cmd, 0, "", "")
        # pip/pipx/uv probes: pretend the CLI is not package-managed.
        return subprocess.CompletedProcess(cmd, 1, "", "not found")

    monkeypatch.setattr(cli_main.subprocess, "run", fake_run)

    res = CliRunner().invoke(cli_main.cli, ["uninstall", "--purge", "--yes"])
    assert res.exit_code == 0, res.output
    # All five daemons booted out of the user's GUI domain.
    assert len(booted) == 5
    assert booted == [f"gui/501/co.ados.{t}" for t in ("supervisor", "control", "compute", "cloud", "logd")]
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

    monkeypatch.setattr(cli_main.platform, "system", lambda: "Darwin")
    monkeypatch.setattr(cli_main.Path, "home", staticmethod(lambda: home))
    monkeypatch.setenv("ADOS_HOME", str(ados_home))
    monkeypatch.setattr(cli_main.os, "getuid", lambda: 501)

    def fake_run(cmd, **_k):
        if cmd[:2] == ["launchctl", "print"]:
            return subprocess.CompletedProcess(cmd, 0, "", "")
        return subprocess.CompletedProcess(cmd, 1, "", "")

    monkeypatch.setattr(cli_main.subprocess, "run", fake_run)

    res = CliRunner().invoke(cli_main.cli, ["uninstall", "--yes"])
    assert res.exit_code == 0, res.output
    assert not list(launch_agents.glob("co.ados.*.plist"))
    assert ados_home.exists()  # identity + config preserved for a re-install
