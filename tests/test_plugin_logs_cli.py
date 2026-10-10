"""``ados plugin logs`` reads the log paths the host reports."""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import patch

import httpx
from click.testing import CliRunner

from ados.cli.plugin import plugin_group

PLUGIN_ID = "com.example.rustplug"


def _run(tmp_path: Path, args: list[str]):
    main_log = tmp_path / "com-example-rustplug-3bb2d8d0.log"
    main_log.write_text("one\ntwo\nthree\n", encoding="utf-8")
    svc_log = tmp_path / "com-example-rustplug-3bb2d8d0-svc.log"
    svc_log.write_text("svc-a\nsvc-b\n", encoding="utf-8")
    seen: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request.url.path)
        return httpx.Response(
            200,
            json={
                "unit": "ados-plugin-com-example-rustplug-3bb2d8d0.service",
                "log_path": str(main_log),
                "service_logs": {"svc": str(svc_log)},
            },
        )

    transport = httpx.MockTransport(handler)
    real_client = httpx.Client

    def client_factory(*a, **kw):
        kw["transport"] = transport
        return real_client(*a, **kw)

    with patch("ados.cli._agent_http.httpx.Client", side_effect=client_factory):
        result = CliRunner().invoke(plugin_group, ["logs", PLUGIN_ID, *args])
    assert seen and seen[0] == f"/api/plugins/{PLUGIN_ID}"
    return result


def test_logs_reads_last_lines_of_host_log_path(tmp_path: Path) -> None:
    result = _run(tmp_path, ["--lines", "2", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["data"]["lines"] == ["two\n", "three\n"]


def test_logs_service_reads_the_service_file(tmp_path: Path) -> None:
    result = _run(tmp_path, ["--service", "svc", "--json"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.output)["data"]["lines"] == ["svc-a\n", "svc-b\n"]


def test_logs_unknown_service_exits_2_with_the_list(tmp_path: Path) -> None:
    result = _run(tmp_path, ["--service", "nope"])
    assert result.exit_code == 2
    assert "svc" in result.output


def test_logs_unreadable_file_exits_permission_denied_with_sudo_hint(tmp_path: Path) -> None:
    # The host's log directory is root-only; an operator account gets a clear
    # refusal and the command to run, not a traceback.
    with patch("ados.cli.plugin.os.access", return_value=False):
        result = _run(tmp_path, ["--lines", "5"])
    assert result.exit_code == 4, result.output
    assert "readable by root only" in result.output
    assert f"sudo ados plugin logs {PLUGIN_ID}" in result.output
    assert "Traceback" not in result.output


def test_logs_unsearchable_directory_is_a_permission_refusal(tmp_path: Path) -> None:
    # Path.exists() raises when the parent directory is not searchable.
    with (
        patch("ados.cli.plugin.Path.exists", side_effect=PermissionError(13, "Permission denied")),
        patch("ados.cli.plugin.os.access", return_value=False),
    ):
        result = _run(tmp_path, ["--lines", "5", "--json"])
    assert result.exit_code == 4, result.output
    assert json.loads(result.output)["ok"] is False
