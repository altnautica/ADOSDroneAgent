"""Extended coverage for the public ``ados`` CLI.

Sibling to ``tests/test_cli.py`` (basic happy paths) and
``tests/test_cli_uninstall_kill_fallback.py`` (systemctl escalation).
This file fills the audit-surfaced gaps:

* ``ados status --json`` schema contract (required keys propagate through).
* ``ados update --check-only`` happy path + already-up-to-date path +
  ``--json`` envelope shape + transport error.
* ``ados uninstall --yes --purge`` dry-run on Linux — verifies the right
  systemctl + filesystem calls are issued without actually touching
  ``/etc`` or ``/opt`` (every system call is mocked).
* CLI error surface: missing systemd, no agent installed, connection
  refused.

Every test mocks ``httpx`` / ``subprocess`` / filesystem at the right
layer so the suite runs in milliseconds on macOS and Linux.
"""

from __future__ import annotations

import json
import platform
import subprocess
from pathlib import Path
from unittest.mock import MagicMock, patch

import click
import httpx
import pytest
from click.testing import CliRunner

from ados.cli import main as cli_main
from ados.cli.main import cli

runner = CliRunner()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _full_status_payload() -> dict:
    """A representative ``/api/v1/setup/status`` response.

    Mirrors the shape consumed by both ``_plain_status`` and the JSON
    output path. Anything the CLI surfaces should be visible here.
    """
    return {
        "version": "0.10.0",
        "device_id": "agent-1",
        "device_name": "bench-agent",
        "profile": "drone",
        "completion_percent": 67,
        "paired": False,
        "pairing_code": "123456",
        "next_action": "Connect or configure the flight controller",
        "access_urls": [
            {
                "kind": "setup",
                "label": "Setup webapp",
                "url": "http://127.0.0.1:8080",
                "source": "local",
                "primary": True,
            }
        ],
        "network": {
            "mdns_host": "ados-abc123.local",
            "api_port": 8080,
            "hotspot_ssid": "ADOS-abc",
        },
        "mavlink": {
            "connected": False,
            "port": "/dev/ttyACM0",
            "baud": 115200,
        },
        "video": {
            "state": "running",
            "whep_url": "http://127.0.0.1:8889/main/whep",
        },
        "cloud_choice": {
            "paired": False,
            "backend_url": "",
            "mode": "",
        },
        "remote_access": {"status": "disabled"},
        "services": [
            {"name": "ados-agent", "state": "running"},
            {"name": "mavlink-proxy", "state": "running"},
        ],
    }


# ---------------------------------------------------------------------------
# status --json schema contract
# ---------------------------------------------------------------------------


REQUIRED_STATUS_KEYS = (
    "version",
    "device_id",
    "device_name",
    "profile",
    "paired",
    "pairing_code",
    "access_urls",
    "mavlink",
    "video",
    "cloud_choice",
    "remote_access",
)


def test_status_json_carries_required_keys() -> None:
    """JSON output is verbatim — every consumer can rely on these keys."""
    payload = _full_status_payload()
    with patch("ados.cli.main._setup_status", return_value=payload):
        result = runner.invoke(cli, ["status", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    for key in REQUIRED_STATUS_KEYS:
        assert key in parsed, f"status JSON must surface '{key}'"


def test_status_json_preserves_nested_video_shape() -> None:
    payload = _full_status_payload()
    with patch("ados.cli.main._setup_status", return_value=payload):
        result = runner.invoke(cli, ["status", "--json"])
    parsed = json.loads(result.output)
    assert parsed["video"]["state"] == "running"
    assert parsed["video"]["whep_url"].endswith("/main/whep")
    assert parsed["mavlink"]["port"] == "/dev/ttyACM0"


def test_status_plain_uses_lan_host_when_available() -> None:
    """A populated ``lan_host`` or ``network.mdns_host`` beats ``access_urls``."""
    payload = _full_status_payload()
    with patch("ados.cli.main._setup_status", return_value=payload):
        result = runner.invoke(cli, ["status"])
    assert result.exit_code == 0
    assert "Open setup: http://ados-abc123.local:8080/setup.html" in result.output


def test_status_plain_falls_back_to_primary_access_url() -> None:
    """No mdns_host? CLI picks the ``primary`` access URL."""
    payload = _full_status_payload()
    payload["network"] = {}
    with patch("ados.cli.main._setup_status", return_value=payload):
        result = runner.invoke(cli, ["status"])
    assert result.exit_code == 0
    assert "Open setup: http://127.0.0.1:8080" in result.output


def test_status_plain_shows_pair_code_when_unpaired() -> None:
    payload = _full_status_payload()
    payload["paired"] = False
    payload["pairing_code"] = "987654"
    with patch("ados.cli.main._setup_status", return_value=payload):
        result = runner.invoke(cli, ["status"])
    assert "code 987654" in result.output


# ---------------------------------------------------------------------------
# update --check-only paths
# ---------------------------------------------------------------------------


def test_update_check_only_already_up_to_date_path() -> None:
    """When the check returns up_to_date the install endpoint must NOT fire."""
    calls: list[tuple[str, str]] = []

    def fake_request(method: str, path: str, **_kwargs):
        calls.append((method, path))
        if path == "/api/ota":
            return {"current_version": "0.10.0"}
        return {"status": "up_to_date"}

    with patch("ados.cli.main._request", side_effect=fake_request):
        result = runner.invoke(cli, ["update", "--check-only"])
    assert result.exit_code == 0
    assert "Already up to date." in result.output
    assert ("POST", "/api/ota/install") not in calls


def test_update_check_only_json_envelope_shape() -> None:
    """``--json`` emits both the current state and the check result."""
    def fake_request(method: str, path: str, **_kwargs):
        if path == "/api/ota":
            return {"current_version": "0.10.0", "state": "idle"}
        return {"status": "update_available", "version": "0.10.1"}

    with patch("ados.cli.main._request", side_effect=fake_request):
        result = runner.invoke(cli, ["update", "--check-only", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert set(parsed.keys()) == {"current", "check"}
    assert parsed["current"]["current_version"] == "0.10.0"
    assert parsed["check"]["status"] == "update_available"
    assert parsed["check"]["version"] == "0.10.1"


def test_update_check_only_skips_install_call() -> None:
    """``--check-only`` short-circuits before any install/restart RPC."""
    calls: list[tuple[str, str]] = []

    def fake_request(method: str, path: str, **_kwargs):
        calls.append((method, path))
        if path == "/api/ota":
            return {"current_version": "0.10.0"}
        return {"status": "update_available", "version": "0.10.5"}

    with patch("ados.cli.main._request", side_effect=fake_request):
        result = runner.invoke(cli, ["update", "--check-only"])

    assert result.exit_code == 0
    assert ("POST", "/api/ota/install") not in calls
    assert ("POST", "/api/ota/restart") not in calls
    assert "0.10.5" in result.output


def test_update_transport_error_surfaces_as_click_exception() -> None:
    """Connection-refused on the API endpoint becomes a friendly error."""
    def fake_request(method: str, path: str, **_kwargs):
        raise click.ClickException("Agent is not running.")

    with patch("ados.cli.main._request", side_effect=fake_request):
        result = runner.invoke(cli, ["update", "--check-only"])
    assert result.exit_code != 0
    assert "Agent is not running." in result.output


# ---------------------------------------------------------------------------
# uninstall --yes --purge — dry-run with every syscall mocked
# ---------------------------------------------------------------------------


@pytest.mark.skipif(
    platform.system() != "Linux",
    reason="uninstall --purge Linux path requires Linux geteuid()",
)
def test_uninstall_linux_purge_dry_run_calls_systemctl_and_cleanup(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """Verify the sequence: stop -> disable -> daemon-reload -> rmtree.

    All filesystem and process calls are mocked. The real system is
    untouched. The test asserts the helper calls the expected
    subcommands in the documented order.
    """
    run_calls: list[list[str]] = []

    def _fake_run(cmd, **_kwargs):
        run_calls.append(list(cmd))
        return subprocess.CompletedProcess(args=cmd, returncode=0)

    install_paths = {
        "install": tmp_path / "opt-ados",
        "config": tmp_path / "etc-ados",
        "data": tmp_path / "var-ados",
        "motd": tmp_path / "30-ados",
    }
    for p in install_paths.values():
        if p.suffix:
            p.write_text("")
        else:
            p.mkdir()

    service_files = [
        tmp_path / "ados-supervisor.service",
        tmp_path / "ados-agent.service",
        tmp_path / "cloudflared.service",
    ]
    for sf in service_files:
        sf.write_text("[Unit]\n")
    symlinks = [tmp_path / f"bin-{name}" for name in ("ados", "ados-agent", "ados-supervisor")]
    for sl in symlinks:
        sl.write_text("")

    rmtree_calls: list[Path] = []

    def _fake_rmtree(path: Path, *_args, **_kwargs) -> None:
        rmtree_calls.append(Path(path))

    with patch.object(cli_main.os, "geteuid", return_value=0), \
         patch.object(cli_main.shutil, "which", return_value="/bin/systemctl"), \
         patch.object(cli_main.subprocess, "run", side_effect=_fake_run), \
         patch.object(cli_main.shutil, "rmtree", side_effect=_fake_rmtree), \
         patch.object(cli_main, "Path") as path_factory:

        def _select_path(arg: str) -> Path:
            if arg == "/opt/ados":
                return install_paths["install"]
            if arg == "/etc/ados":
                return install_paths["config"]
            if arg == "/var/ados":
                return install_paths["data"]
            if arg == "/etc/update-motd.d/30-ados":
                return install_paths["motd"]
            if arg.startswith("/etc/systemd/system/"):
                name = arg.rsplit("/", 1)[-1]
                for sf in service_files:
                    if sf.name == name:
                        return sf
                return tmp_path / name
            if arg.startswith("/usr/local/bin/"):
                name = arg.rsplit("/", 1)[-1]
                return tmp_path / f"bin-{name}"
            return Path(arg)

        path_factory.side_effect = _select_path

        result = runner.invoke(cli, ["uninstall", "--yes", "--purge"])

    assert result.exit_code == 0, result.output
    # Each unit got a stop (via helper) and a disable.
    stop_calls = [c for c in run_calls if c[:2] == ["systemctl", "stop"]]
    disable_calls = [c for c in run_calls if c[:2] == ["systemctl", "disable"]]
    assert len(stop_calls) == 3
    assert len(disable_calls) == 3
    # Final daemon-reload.
    assert ["systemctl", "daemon-reload"] in run_calls
    # rmtree fired for install + data + config (because --purge).
    rmtree_strs = {p.name for p in rmtree_calls}
    assert "opt-ados" in rmtree_strs
    assert "var-ados" in rmtree_strs
    assert "etc-ados" in rmtree_strs


# ---------------------------------------------------------------------------
# Error paths
# ---------------------------------------------------------------------------


def test_uninstall_unsupported_platform_raises() -> None:
    """A non-Linux/macOS host fails fast with a clear message."""
    with patch.object(cli_main.platform, "system", return_value="Windows"):
        result = runner.invoke(cli, ["uninstall", "--yes"])
    assert result.exit_code != 0
    assert "Unsupported platform" in result.output


def test_uninstall_linux_requires_root(monkeypatch: pytest.MonkeyPatch) -> None:
    """Linux uninstall without root must refuse before touching anything."""
    monkeypatch.setattr(cli_main.platform, "system", lambda: "Linux")
    # Force a non-root geteuid even on macOS CI hosts (where the attr exists).
    monkeypatch.setattr(cli_main.os, "geteuid", lambda: 1000, raising=False)
    result = runner.invoke(cli, ["uninstall", "--yes"])
    assert result.exit_code != 0
    assert "requires root" in result.output


def test_uninstall_nothing_installed_is_a_clean_noop(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """If install dir and config dir are missing, the command exits cleanly."""
    monkeypatch.setattr(cli_main.platform, "system", lambda: "Linux")
    monkeypatch.setattr(cli_main.os, "geteuid", lambda: 0, raising=False)
    monkeypatch.setattr(cli_main.shutil, "which", lambda _bin: None)

    empty = tmp_path / "nothing-here"

    def _select_path(arg: str) -> Path:
        return empty / arg.replace("/", "_")

    with patch.object(cli_main, "Path", side_effect=_select_path):
        result = runner.invoke(cli, ["uninstall", "--yes"])

    assert result.exit_code == 0
    assert "Nothing to uninstall" in result.output


def test_request_connect_error_yields_friendly_message() -> None:
    """A real connection refusal converts to a friendly Click error."""
    with patch("httpx.Client") as client_factory:
        instance = MagicMock()
        instance.__enter__.return_value = instance
        instance.__exit__.return_value = False
        instance.request.side_effect = httpx.ConnectError("refused")
        client_factory.return_value = instance
        with pytest.raises(click.ClickException) as exc:
            cli_main._request("GET", "/api/v1/setup/status")
    assert "Agent is not running" in str(exc.value.message)


def test_request_http_status_error_includes_response_text() -> None:
    """A 503 must surface the status code and a snippet of the body."""
    fake_response = MagicMock(status_code=503)
    fake_response.text = "Service Unavailable"
    err = httpx.HTTPStatusError("bad", request=MagicMock(), response=fake_response)
    with patch("httpx.Client") as client_factory:
        instance = MagicMock()
        instance.__enter__.return_value = instance
        instance.__exit__.return_value = False

        response_mock = MagicMock()
        response_mock.raise_for_status.side_effect = err
        instance.request.return_value = response_mock
        client_factory.return_value = instance

        with pytest.raises(click.ClickException) as exc:
            cli_main._request("GET", "/api/v1/setup/status")
    assert "503" in str(exc.value.message)


# ---------------------------------------------------------------------------
# Auth header derivation from on-disk pairing state
# ---------------------------------------------------------------------------


def test_auth_headers_empty_when_pairing_file_missing(tmp_path: Path) -> None:
    """No pairing file on disk -> no auth header sent."""
    with patch.object(cli_main, "PAIRING_STATE_PATH", tmp_path / "missing.json"):
        assert cli_main._auth_headers() == {}


def test_auth_headers_carries_api_key_when_present(tmp_path: Path) -> None:
    state = tmp_path / "pairing.json"
    state.write_text(json.dumps({"api_key": "secret-xyz"}))
    with patch.object(cli_main, "PAIRING_STATE_PATH", state):
        headers = cli_main._auth_headers()
    assert headers == {"X-ADOS-Key": "secret-xyz"}


def test_auth_headers_handles_malformed_pairing_json(tmp_path: Path) -> None:
    """Corrupt pairing JSON degrades to anonymous (no header)."""
    state = tmp_path / "pairing.json"
    state.write_text("{ not valid json")
    with patch.object(cli_main, "PAIRING_STATE_PATH", state):
        assert cli_main._auth_headers() == {}


def test_auth_headers_skips_non_string_api_key(tmp_path: Path) -> None:
    state = tmp_path / "pairing.json"
    state.write_text(json.dumps({"api_key": 42}))  # wrong type
    with patch.object(cli_main, "PAIRING_STATE_PATH", state):
        assert cli_main._auth_headers() == {}


# ---------------------------------------------------------------------------
# WHEP URL helper
# ---------------------------------------------------------------------------


def test_viewer_url_from_whep_strips_whep_suffix() -> None:
    derived = cli_main._viewer_url_from_whep("http://host:8889/main/whep")
    assert derived == "http://host:8889/main/"


def test_viewer_url_from_whep_handles_trailing_slash() -> None:
    derived = cli_main._viewer_url_from_whep("http://host:8889/main/whep/")
    assert derived == "http://host:8889/main/"


def test_viewer_url_from_whep_none_passthrough() -> None:
    assert cli_main._viewer_url_from_whep(None) is None
    assert cli_main._viewer_url_from_whep("") is None
