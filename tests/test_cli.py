"""Tests for the minimal public ADOS CLI."""

from __future__ import annotations

from unittest.mock import patch

from click.testing import CliRunner

from ados.cli.main import cli

runner = CliRunner()


def _setup_payload() -> dict:
    return {
        "version": "0.10.0",
        "device_id": "agent-1",
        "device_name": "bench-agent",
        "profile": "drone",
        "completion_percent": 67,
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
        "mavlink": {"connected": False, "port": "", "baud": 0},
        "video": {"state": "running", "whep_url": "http://127.0.0.1:8889/main/whep"},
        "remote_access": {"status": "disabled"},
    }


def test_help_shows_only_public_commands() -> None:
    result = runner.invoke(cli, ["--help"])
    assert result.exit_code == 0
    assert "status" in result.output
    assert "update" in result.output
    assert "uninstall" in result.output
    assert "tui" not in result.output
    assert "config" not in result.output
    assert "gs" not in result.output
    assert "plugin" not in result.output


def test_status_prints_setup_summary() -> None:
    with patch("ados.cli.main._setup_status", return_value=_setup_payload()):
        result = runner.invoke(cli, ["status"])
    assert result.exit_code == 0
    assert "bench-agent" in result.output
    assert "Open setup: http://127.0.0.1:8080" in result.output
    assert "Video:   running" in result.output


def test_status_json_outputs_full_payload() -> None:
    with patch("ados.cli.main._setup_status", return_value=_setup_payload()):
        result = runner.invoke(cli, ["status", "--json"])
    assert result.exit_code == 0
    assert '"device_id": "agent-1"' in result.output


def test_root_command_falls_back_to_plain_status_in_non_tty() -> None:
    with patch("ados.cli.main._setup_status", return_value=_setup_payload()):
        result = runner.invoke(cli, [])
    assert result.exit_code == 0
    assert "Open setup:" in result.output


def test_update_check_only_does_not_install() -> None:
    calls: list[tuple[str, str]] = []

    def fake_request(method: str, path: str, **_kwargs):
        calls.append((method, path))
        if path == "/api/ota":
            return {"current_version": "0.10.0"}
        return {"status": "update_available", "version": "0.10.1"}

    with patch("ados.cli.main._request", side_effect=fake_request):
        result = runner.invoke(cli, ["update", "--check-only"])
    assert result.exit_code == 0
    assert "Update available: 0.10.1" in result.output
    assert ("POST", "/api/ota/install") not in calls


def test_demo_uses_user_writable_pairing_state(tmp_path) -> None:
    """Demo mode remains available as a hidden no-hardware development path."""
    from ados.core.config import ADOSConfig

    config = ADOSConfig()
    apps = []

    class FakeAgentApp:
        def __init__(self, app_config, demo):
            self.config = app_config
            self.demo = demo
            apps.append(self)

        def request_shutdown(self):
            pass

        async def start(self):
            return None

    with (
        patch("pathlib.Path.home", return_value=tmp_path),
        patch("ados.core.config.load_config", return_value=config),
        patch("ados.core.logging.configure_logging"),
        patch("ados.core.main.AgentApp", FakeAgentApp),
    ):
        result = runner.invoke(cli, ["demo", "--port", "18080"])

    assert result.exit_code == 0
    assert apps
    assert apps[0].demo is True
    assert config.pairing.state_path == str(tmp_path / ".ados" / "demo-pairing.json")
    assert config.pairing.convex_url == ""


def test_install_command_is_registered() -> None:
    result = runner.invoke(cli, ["--help"])
    assert result.exit_code == 0
    assert "install" in result.output


def test_install_help_documents_status_and_resume() -> None:
    result = runner.invoke(cli, ["install", "--help"])
    assert result.exit_code == 0
    assert "--status" in result.output
    assert "--resume" in result.output


def test_install_status_reports_no_result_when_absent(tmp_path) -> None:
    import ados.cli.main as cli_main

    missing_result = tmp_path / "install-result.json"
    missing_cp = tmp_path / "install-checkpoints"
    with (
        patch.object(cli_main, "INSTALL_RESULT_PATH", missing_result),
        patch.object(cli_main, "INSTALL_CHECKPOINT_DIR", missing_cp),
    ):
        result = runner.invoke(cli, ["install", "--status"])
    assert result.exit_code == 0
    assert "No install result recorded" in result.output
    # Every REQUIRED step shows as missing when nothing has run.
    assert "Checkpoints missing (5)" in result.output


def test_install_status_json_reports_done_and_missing(tmp_path) -> None:
    import json

    import ados.cli.main as cli_main

    result_file = tmp_path / "install-result.json"
    result_file.write_text(
        json.dumps(
            {
                "status": "degraded",
                "version": "1.2.3",
                "profile": "drone",
                "board": "Reference",
                "kernelRelease": "6.1.0",
                "wfbModuleSource": "dkms",
                "failedSteps": ["radio-driver"],
                "requiredFailures": [],
                "ts": "2026-05-25T10:00:00Z",
            }
        ),
        encoding="utf-8",
    )
    cp_dir = tmp_path / "install-checkpoints"
    cp_dir.mkdir()
    (cp_dir / "deps.done").touch()
    (cp_dir / "venv.done").touch()

    with (
        patch.object(cli_main, "INSTALL_RESULT_PATH", result_file),
        patch.object(cli_main, "INSTALL_CHECKPOINT_DIR", cp_dir),
    ):
        result = runner.invoke(cli, ["install", "--status", "--json"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["result"]["status"] == "degraded"
    assert payload["checkpoints"]["done"] == ["deps", "venv"]
    assert payload["checkpoints"]["missing"] == [
        "agent-package",
        "systemd",
        "global-symlinks",
    ]


def test_install_resume_is_linux_only() -> None:
    with patch("ados.cli.main.platform.system", return_value="Darwin"):
        result = runner.invoke(cli, ["install", "--resume"])
    assert result.exit_code != 0
    assert "only supported on Linux" in result.output
