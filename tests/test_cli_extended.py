"""Extended coverage for the public ``ados`` CLI.

Sibling to ``tests/test_cli.py`` (basic happy paths). Covers:

* ``ados status --json`` schema contract (required keys propagate through).
* ``ados update --check-only`` happy path + already-up-to-date path +
  ``--json`` envelope shape + transport error.
* ``ados uninstall`` on Linux routes to the installer's ``--uninstall``: the
  copy kept on the box first (offline), else a fetched install.sh, else a
  clear error (every process and network call is mocked).
* CLI error surface: missing systemd, no agent installed, connection
  refused.

Every test mocks ``httpx`` / ``subprocess`` / filesystem at the right
layer so the suite runs in milliseconds on macOS and Linux.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from types import SimpleNamespace
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
    assert "Reach this agent" in result.output
    assert "ados-abc123.local:8080" in result.output


def test_status_plain_falls_back_to_primary_access_url() -> None:
    """No mdns_host? CLI picks the ``primary`` access URL."""
    payload = _full_status_payload()
    payload["network"] = {}
    with patch("ados.cli.main._setup_status", return_value=payload):
        result = runner.invoke(cli, ["status"])
    assert result.exit_code == 0
    assert "127.0.0.1:8080" in result.output


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


def test_update_already_up_to_date_skips_upgrade() -> None:
    """When main is at the installed version, the upgrade must NOT run."""
    with (
        patch("ados.cli.main._installed_version", return_value="0.10.0"),
        patch("ados.cli.main._latest_main_version", return_value="0.10.0"),
        patch("ados.cli.main._run_upgrade") as upgrade,
    ):
        result = runner.invoke(cli, ["update"])
    assert result.exit_code == 0
    assert "Already up to date." in result.output
    upgrade.assert_not_called()


def test_update_json_envelope_shape() -> None:
    """``--json`` emits current + latest + update_available."""
    with (
        patch("ados.cli.main._installed_version", return_value="0.10.0"),
        patch("ados.cli.main._latest_main_version", return_value="0.10.1"),
    ):
        result = runner.invoke(cli, ["update", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert set(parsed.keys()) == {"current_version", "latest_version", "update_available"}
    assert parsed["current_version"] == "0.10.0"
    assert parsed["latest_version"] == "0.10.1"
    assert parsed["update_available"] is True


def test_update_check_only_skips_upgrade() -> None:
    """``--check-only`` reports versions and never runs the upgrade."""
    with (
        patch("ados.cli.main._installed_version", return_value="0.10.0"),
        patch("ados.cli.main._latest_main_version", return_value="0.10.5"),
        patch("ados.cli.main._run_upgrade") as upgrade,
    ):
        result = runner.invoke(cli, ["update", "--check-only"])
    assert result.exit_code == 0
    upgrade.assert_not_called()
    assert "0.10.5" in result.output


def test_update_upgrade_failure_surfaces_as_click_exception() -> None:
    """A failure fetching/running the installer surfaces as a friendly error."""
    with (
        patch("ados.cli.main._installed_version", return_value="0.10.0"),
        patch("ados.cli.main._latest_main_version", return_value="0.10.1"),
        patch(
            "ados.cli.main._run_upgrade",
            side_effect=click.ClickException("Could not fetch the installer"),
        ),
    ):
        result = runner.invoke(cli, ["update", "--yes"])
    assert result.exit_code != 0
    assert "Could not fetch the installer" in result.output


# ---------------------------------------------------------------------------
# uninstall — the Rust installer's `--uninstall` is the only removal path
# ---------------------------------------------------------------------------


def test_uninstall_linux_runs_the_kept_installer_without_the_network(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """With the installer copy on the box, uninstall runs it and never fetches."""
    local = tmp_path / "ados-installer"
    local.write_text("#!/bin/sh\n")
    local.chmod(0o755)
    monkeypatch.setattr(cli_main.platform, "system", lambda: "Linux")
    monkeypatch.setattr(cli_main, "LOCAL_INSTALLER_PATH", local)
    monkeypatch.setattr(cli_main.os, "geteuid", lambda: 0, raising=False)
    run_calls: list[list[str]] = []

    def _fake_run(cmd, **_kwargs):
        run_calls.append(list(cmd))
        return subprocess.CompletedProcess(args=cmd, returncode=0)

    def _no_network(*_a, **_kw):
        raise AssertionError("the kept installer must not need the network")

    with patch.object(cli_main.subprocess, "run", side_effect=_fake_run), \
         patch.object(cli_main.httpx, "Client", side_effect=_no_network):
        result = runner.invoke(cli, ["uninstall", "--yes", "--purge"])

    assert result.exit_code == 0, result.output
    assert run_calls == [[str(local), "--uninstall", "--force"]]


def test_uninstall_linux_without_a_kept_installer_runs_install_sh(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """A box without the installer copy fetches install.sh and passes the flags."""
    monkeypatch.setattr(cli_main.platform, "system", lambda: "Linux")
    monkeypatch.setattr(cli_main, "LOCAL_INSTALLER_PATH", tmp_path / "absent")
    monkeypatch.setattr(cli_main.os, "geteuid", lambda: 0, raising=False)
    run_calls: list[list[str]] = []

    def _fake_run(cmd, **_kwargs):
        run_calls.append(list(cmd))
        return subprocess.CompletedProcess(args=cmd, returncode=0)

    client = MagicMock()
    client.__enter__.return_value = client
    client.__exit__.return_value = False
    client.get.return_value = MagicMock(text="#!/bin/sh\n")
    with patch.object(cli_main.subprocess, "run", side_effect=_fake_run), \
         patch.object(cli_main.httpx, "Client", return_value=client):
        result = runner.invoke(cli, ["uninstall", "--yes"])

    assert result.exit_code == 0, result.output
    assert len(run_calls) == 1
    assert run_calls[0][0] == "bash"
    assert run_calls[0][2:] == ["--uninstall"]


def test_uninstall_linux_offline_without_a_kept_installer_says_what_to_do(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """No installer copy and no network: a clear error, nothing half-removed."""
    monkeypatch.setattr(cli_main.platform, "system", lambda: "Linux")
    monkeypatch.setattr(cli_main, "LOCAL_INSTALLER_PATH", tmp_path / "absent")
    monkeypatch.setattr(cli_main.os, "geteuid", lambda: 0, raising=False)
    client = MagicMock()
    client.__enter__.return_value = client
    client.__exit__.return_value = False
    client.get.side_effect = httpx.ConnectError("offline")
    with patch.object(cli_main.subprocess, "run") as run, \
         patch.object(cli_main.httpx, "Client", return_value=client):
        result = runner.invoke(cli, ["uninstall", "--yes"])

    assert result.exit_code != 0
    assert "could not be downloaded" in result.output
    run.assert_not_called()


# ---------------------------------------------------------------------------
# Error paths
# ---------------------------------------------------------------------------


def test_uninstall_unsupported_platform_raises() -> None:
    """A non-Linux/macOS host fails fast with a clear message."""
    with patch.object(cli_main.platform, "system", return_value="Windows"):
        result = runner.invoke(cli, ["uninstall", "--yes"])
    assert result.exit_code != 0
    assert "Unsupported platform" in result.output


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


def test_viewer_url_from_whep_relative_points_at_local_mediamtx() -> None:
    # The agent now advertises a same-origin relative path; the on-box CLI cannot
    # build a browser-clickable absolute URL from it, so it falls back to the
    # local mediamtx viewer.
    assert cli_main._viewer_url_from_whep("/whep") == "http://127.0.0.1:8889/main/"


def test_update_pins_the_boxs_own_profile_so_it_cannot_be_reprofiled() -> None:
    """An upgrade must never change what a box IS.

    The installer resolves the profile itself when none is passed, and a gap in
    that resolution re-profiled a live ground station to ``drone`` and left it
    in a reboot loop that needed a reflash. The box already knows what it is,
    so the upgrade states it explicitly instead of trusting a default.
    """
    with (
        patch("ados.cli.main._installed_version", return_value="0.10.0"),
        patch("ados.cli.main._latest_main_version", return_value="0.10.1"),
        patch("ados.cli.main._read_profile_conf_value", return_value="ground_station"),
        patch("ados.cli.main._run_upgrade") as upgrade,
    ):
        result = runner.invoke(cli, ["update", "--yes"])
    assert result.exit_code == 0
    upgrade.assert_called_once_with("ground_station")


def test_update_profile_override_wins_for_a_deliberate_conversion() -> None:
    """The one legitimate reason to change a box's role: an explicit flag."""
    with (
        patch("ados.cli.main._installed_version", return_value="0.10.0"),
        patch("ados.cli.main._latest_main_version", return_value="0.10.1"),
        patch("ados.cli.main._read_profile_conf_value", return_value="drone"),
        patch("ados.cli.main._run_upgrade") as upgrade,
    ):
        result = runner.invoke(cli, ["update", "--yes", "--profile", "ground-station"])
    assert result.exit_code == 0
    upgrade.assert_called_once_with("ground-station")


def test_update_says_so_when_the_profile_cannot_be_known() -> None:
    """No marker on disk: the operator is told, rather than it passing silently."""
    with (
        patch("ados.cli.main._installed_version", return_value="0.10.0"),
        patch("ados.cli.main._latest_main_version", return_value="0.10.1"),
        patch("ados.cli.main._read_profile_conf_value", return_value=None),
        patch("ados.cli.main._run_upgrade") as upgrade,
    ):
        result = runner.invoke(cli, ["update", "--yes"])
    assert result.exit_code == 0
    upgrade.assert_called_once_with(None)
    assert "unknown" in result.output.lower()

def test_run_upgrade_actually_passes_the_profile_to_the_installer() -> None:
    """The decision is worthless unless it reaches the installer's argv.

    Deciding the profile and then not passing it is exactly the bug this fixes,
    so assert on the command that is actually executed, not on the call that
    computes it.
    """
    seen: dict[str, list[str]] = {}

    class _Resp:
        text = "#!/bin/sh\n"

        def raise_for_status(self) -> None:
            return None

    class _Client:
        def __init__(self, *_a, **_kw) -> None:
            return None

        def __enter__(self):
            return self

        def __exit__(self, *a) -> None:
            return None

        def get(self, _url):
            return _Resp()

    def _fake_run(argv, **_kw):
        seen["argv"] = list(argv)
        return SimpleNamespace(returncode=0)

    with (
        patch("ados.cli.main.httpx.Client", _Client),
        patch("ados.cli.main.subprocess.run", _fake_run),
        patch("ados.cli.main.os.geteuid", return_value=0),
        patch("ados.cli.main.platform.system", return_value="Linux"),
    ):
        cli_main._run_upgrade("ground_station")

    argv = seen["argv"]
    assert "--upgrade" in argv
    assert "--profile" in argv, f"the installer was invoked without a profile: {argv}"
    assert argv[argv.index("--profile") + 1] == "ground_station"


def test_run_upgrade_omits_the_flag_when_the_profile_is_unknown() -> None:
    """No marker on disk: fall back to the installer's own resolution rather
    than inventing a profile, which would be the same class of bug."""
    seen: dict[str, list[str]] = {}

    class _Resp:
        text = "#!/bin/sh\n"

        def raise_for_status(self) -> None:
            return None

    class _Client:
        def __init__(self, *_a, **_kw) -> None:
            return None

        def __enter__(self):
            return self

        def __exit__(self, *a) -> None:
            return None

        def get(self, _url):
            return _Resp()

    def _fake_run(argv, **_kw):
        seen["argv"] = list(argv)
        return SimpleNamespace(returncode=0)

    with (
        patch("ados.cli.main.httpx.Client", _Client),
        patch("ados.cli.main.subprocess.run", _fake_run),
        patch("ados.cli.main.os.geteuid", return_value=0),
        patch("ados.cli.main.platform.system", return_value="Linux"),
    ):
        cli_main._run_upgrade(None)

    assert "--profile" not in seen["argv"]
