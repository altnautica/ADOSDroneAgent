"""Plugin-declared supervised services: manifest coercion + supervisor lifecycle.

Covers the additive ``agent.contributes.services`` shape: the legacy
``list[str]`` form still parses, the rich ``list[ServiceSpec]`` form
parses, the supervisor renders/starts/stops extra units alongside the
main runner unit, and the readiness probe + persisted ``service_status``
round-trip cleanly.
"""

from __future__ import annotations

import zipfile
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from ados.plugins.archive import MANIFEST_FILENAME
from ados.plugins.manifest import AgentContributes, PluginManifest, ServiceSpec
from ados.plugins.supervisor import PluginSupervisor

# ---------------------------------------------------------------------
# Manifest coercion + parsing
# ---------------------------------------------------------------------


def test_legacy_string_services_still_parse() -> None:
    """Old ``services: ["foo"]`` coerces each bare string to a spec."""
    c = AgentContributes.model_validate({"services": ["foo", "bar"]})
    assert [s.name for s in c.services] == ["foo", "bar"]
    # The bare string becomes both the name and the exec command.
    assert c.services[0].command == "foo"
    assert c.services[0].ready_check is None
    assert c.services[0].restart == "on-failure"
    assert c.services[0].slice == "ados-plugins.slice"


def test_rich_service_specs_parse() -> None:
    c = AgentContributes.model_validate(
        {
            "services": [
                {
                    "name": "sensor-daemon",
                    "command": "/opt/ados/plugins/com.example.x/bin/run",
                    "ready_check": "http://127.0.0.1:9100/healthz",
                    "restart": "always",
                },
            ]
        }
    )
    assert len(c.services) == 1
    spec = c.services[0]
    assert spec.name == "sensor-daemon"
    assert spec.command.endswith("/bin/run")
    assert spec.ready_check == "http://127.0.0.1:9100/healthz"
    assert spec.restart == "always"


def test_mixed_legacy_and_rich_services_parse() -> None:
    c = AgentContributes.model_validate(
        {"services": ["legacy", {"name": "rich", "command": "echo hi"}]}
    )
    assert [s.name for s in c.services] == ["legacy", "rich"]
    assert c.services[0].command == "legacy"
    assert c.services[1].command == "echo hi"


def test_empty_services_default() -> None:
    c = AgentContributes.model_validate({})
    assert c.services == []
    assert c.service_specs() == []


def test_service_name_rejects_uppercase() -> None:
    from ados.plugins.errors import ManifestError

    with pytest.raises(ManifestError):
        ServiceSpec.model_validate({"name": "BadName", "command": "x"})


def test_service_command_required() -> None:
    # A rich entry must carry a non-empty command.
    with pytest.raises(Exception):
        ServiceSpec.model_validate({"name": "ok"})


def test_full_manifest_with_services_parses() -> None:
    manifest = PluginManifest.from_yaml_text(
        """\
schema_version: 1
id: com.example.daemonplug
version: 0.1.0
name: Daemon Plug
license: GPL-3.0-or-later
risk: medium
compatibility:
  ados_version: ">=0.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions: ["event.publish"]
  contributes:
    services:
      - name: worker
        command: /opt/ados/plugins/com.example.daemonplug/bin/worker
        ready_check: "cmd:/bin/true"
        restart: always
"""
    )
    specs = manifest.agent.contributes.services
    assert len(specs) == 1
    assert specs[0].name == "worker"
    assert specs[0].ready_check == "cmd:/bin/true"


# ---------------------------------------------------------------------
# Systemd rendering
# ---------------------------------------------------------------------


def _service_manifest() -> PluginManifest:
    return PluginManifest.from_yaml_text(
        """\
schema_version: 1
id: com.example.daemonplug
version: 0.1.0
name: Daemon Plug
license: GPL-3.0-or-later
risk: medium
compatibility:
  ados_version: ">=0.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions: ["event.publish"]
  resources:
    max_ram_mb: 48
    max_cpu_percent: 20
    max_pids: 6
  contributes:
    services:
      - name: worker
        command: /opt/ados/plugins/com.example.daemonplug/bin/worker --flag
        ready_check: "http://127.0.0.1:9100/healthz"
        restart: always
"""
    )


def test_service_unit_naming_does_not_collide_with_main() -> None:
    from ados.plugins.systemd import service_unit_name_for, unit_name_for

    main = unit_name_for("com.example.daemonplug")
    svc = service_unit_name_for("com.example.daemonplug", "worker")
    assert main == "ados-plugin-com-example-daemonplug.service"
    assert svc == "ados-plugin-com-example-daemonplug-worker.service"
    assert main != svc


def test_render_service_unit_emits_command_and_limits() -> None:
    from ados.plugins.systemd import render_service_unit

    manifest = _service_manifest()
    spec = manifest.agent.contributes.services[0]
    unit = render_service_unit(manifest, spec, Path("/var/ados/plugins"))
    assert (
        "ExecStart=/opt/ados/plugins/com.example.daemonplug/bin/worker --flag"
        in unit
    )
    assert "Restart=always" in unit
    assert "Slice=ados-plugins.slice" in unit
    assert "MemoryMax=48M" in unit
    assert "CPUQuota=20%" in unit
    assert "TasksMax=6" in unit
    assert "NoNewPrivileges=yes" in unit
    assert (
        "WorkingDirectory=/var/ados/plugins/com.example.daemonplug" in unit
    )


# ---------------------------------------------------------------------
# Supervisor lifecycle: declared service units start/stop/remove
# ---------------------------------------------------------------------


def _build_service_archive(tmp_path: Path) -> Path:
    manifest_yaml = """\
schema_version: 1
id: com.example.daemonplug
version: 0.1.0
name: Daemon Plug
license: GPL-3.0-or-later
risk: medium
compatibility:
  ados_version: ">=0.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions: ["event.publish"]
  resources:
    max_ram_mb: 48
    max_cpu_percent: 20
    max_pids: 6
  contributes:
    services:
      - name: worker
        command: /bin/true
"""
    archive_path = tmp_path / "com.example.daemonplug.adosplug"
    with zipfile.ZipFile(archive_path, "w") as zf:
        zf.writestr(MANIFEST_FILENAME, manifest_yaml)
        zf.writestr("agent/plugin.py", "# stub\n")
    return archive_path


@pytest.fixture
def isolated_paths(tmp_path: Path, monkeypatch):
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    state_path = state_dir / "plugin-state.json"
    keys_dir = tmp_path / "keys"
    keys_dir.mkdir()
    log_dir = tmp_path / "log"
    log_dir.mkdir()
    unit_dir = tmp_path / "systemd"
    unit_dir.mkdir()

    monkeypatch.setattr(
        "ados.plugins.state.PLUGIN_STATE_PATH", state_path, raising=False
    )
    monkeypatch.setattr(
        "ados.plugins.signing.PLUGIN_KEYS_DIR", keys_dir, raising=False
    )
    monkeypatch.setattr(
        "ados.plugins.systemd.PLUGIN_UNIT_DIR", unit_dir, raising=False
    )
    monkeypatch.setattr(
        "ados.plugins.systemd.PLUGIN_LOG_DIR", log_dir, raising=False
    )
    from ados.plugins import systemd as systemd_mod

    monkeypatch.setattr(
        systemd_mod,
        "PLUGIN_SLICE_PATH",
        unit_dir / systemd_mod.PLUGIN_SLICE_NAME,
        raising=False,
    )
    return {
        "install_dir": tmp_path / "var-plugins",
        "unit_dir": unit_dir,
        "state_path": state_path,
    }


def test_enable_writes_extra_service_unit(isolated_paths, tmp_path: Path):
    archive = _build_service_archive(tmp_path)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    sup.discover()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="", stdout="")
        sup.install_archive(archive)
        sup.grant_permission("com.example.daemonplug", "event.publish")
        sup.enable("com.example.daemonplug")

    main_unit = (
        isolated_paths["unit_dir"]
        / "ados-plugin-com-example-daemonplug.service"
    )
    svc_unit = (
        isolated_paths["unit_dir"]
        / "ados-plugin-com-example-daemonplug-worker.service"
    )
    assert main_unit.exists()
    assert svc_unit.exists()
    # The declared service was enabled + started via systemctl.
    started = [
        c.args[0]
        for c in run_mock.call_args_list
        if "start" in c.args[0]
        and "ados-plugin-com-example-daemonplug-worker.service" in c.args[0]
    ]
    assert started, "expected the worker service unit to be started"


def test_enable_persists_service_readiness(isolated_paths, tmp_path: Path):
    archive = _build_service_archive(tmp_path)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    sup.discover()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        # systemctl calls succeed; the readiness probe (no ready_check)
        # runs `systemctl is-active --quiet` which also returns 0 here.
        run_mock.return_value = MagicMock(returncode=0, stderr="", stdout="")
        sup.install_archive(archive)
        sup.grant_permission("com.example.daemonplug", "event.publish")
        sup.enable("com.example.daemonplug")

    install = sup.find_install("com.example.daemonplug")
    assert install is not None
    assert install.service_status is not None
    entry = install.service_status[0]
    assert entry["name"] == "worker"
    assert entry["ready"] is True
    assert entry["reason"] is None


def test_disable_stops_services_and_clears_readiness(
    isolated_paths, tmp_path: Path
):
    archive = _build_service_archive(tmp_path)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    sup.discover()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="", stdout="")
        sup.install_archive(archive)
        sup.grant_permission("com.example.daemonplug", "event.publish")
        sup.enable("com.example.daemonplug")
        sup.disable("com.example.daemonplug")
        stopped = [
            c.args[0]
            for c in run_mock.call_args_list
            if "stop" in c.args[0]
            and "ados-plugin-com-example-daemonplug-worker.service"
            in c.args[0]
        ]
    assert stopped, "expected the worker service unit to be stopped"
    install = sup.find_install("com.example.daemonplug")
    assert install is not None
    assert install.service_status is None


def test_remove_deletes_service_unit_files(isolated_paths, tmp_path: Path):
    archive = _build_service_archive(tmp_path)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    sup.discover()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="", stdout="")
        sup.install_archive(archive)
        sup.grant_permission("com.example.daemonplug", "event.publish")
        sup.enable("com.example.daemonplug")
        sup.remove("com.example.daemonplug", keep_data=True)
    svc_unit = (
        isolated_paths["unit_dir"]
        / "ados-plugin-com-example-daemonplug-worker.service"
    )
    assert not svc_unit.exists()
    assert sup.installs() == []


def test_readiness_not_ready_when_unit_inactive(isolated_paths, tmp_path: Path):
    archive = _build_service_archive(tmp_path)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    sup.discover()

    def fake_run(args, **kwargs):
        # is-active probe fails (unit not up); lifecycle systemctl succeeds.
        if "is-active" in args:
            return MagicMock(returncode=3, stderr="", stdout="")
        return MagicMock(returncode=0, stderr="", stdout="")

    with patch("ados.plugins.supervisor.subprocess.run", side_effect=fake_run):
        sup.install_archive(archive)
        sup.grant_permission("com.example.daemonplug", "event.publish")
        sup.enable("com.example.daemonplug")
        readiness = sup.readiness_for("com.example.daemonplug")
    assert readiness == [
        {"name": "worker", "ready": False, "reason": "unit not active"}
    ]


def test_readiness_for_unknown_plugin_raises(isolated_paths):
    from ados.plugins.errors import SupervisorError

    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    sup.discover()
    with pytest.raises(SupervisorError):
        sup.readiness_for("com.example.absent")


def test_command_ready_check_exit_zero_is_ready(isolated_paths, tmp_path: Path):
    """A `cmd:`-style (non-URL) ready_check runs as a shell command."""
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    spec = ServiceSpec.model_validate(
        # `exit 0` is a shell builtin, portable across the test hosts.
        {"name": "worker", "command": "noop", "ready_check": "exit 0"}
    )
    ready, reason = sup._probe_service_ready("com.example.x", spec)
    assert ready is True
    assert reason is None


def test_command_ready_check_nonzero_is_not_ready(
    isolated_paths, tmp_path: Path
):
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    spec = ServiceSpec.model_validate(
        {"name": "worker", "command": "noop", "ready_check": "exit 7"}
    )
    ready, reason = sup._probe_service_ready("com.example.x", spec)
    assert ready is False
    assert reason is not None and "exit" in reason


# ---------------------------------------------------------------------
# State round-trip of service_status
# ---------------------------------------------------------------------


def test_service_status_round_trips_through_state(tmp_path: Path):
    from ados.plugins.state import PluginInstall, load_state, save_state

    state_path = tmp_path / "plugin-state.json"
    inst = PluginInstall(
        plugin_id="com.example.daemonplug",
        version="0.1.0",
        source="local_file",
        source_uri=None,
        signer_id=None,
        manifest_hash="deadbeef",
        status="running",
        installed_at=1,
        service_status=[{"name": "worker", "ready": True, "reason": None}],
    )
    save_state([inst], path=state_path)
    loaded = load_state(path=state_path)
    assert len(loaded) == 1
    assert loaded[0].service_status == [
        {"name": "worker", "ready": True, "reason": None}
    ]


def test_old_state_file_loads_with_none_service_status(tmp_path: Path):
    """A state file written before this field loads with None."""
    from ados.plugins.state import load_state

    state_path = tmp_path / "plugin-state.json"
    state_path.write_text(
        """\
{
  "schema": 1,
  "installs": [
    {
      "plugin_id": "com.example.old",
      "version": "0.1.0",
      "source": "local_file",
      "source_uri": null,
      "signer_id": null,
      "manifest_hash": "abc",
      "status": "running",
      "installed_at": 1
    }
  ]
}
""",
        encoding="utf-8",
    )
    loaded = load_state(path=state_path)
    assert len(loaded) == 1
    assert loaded[0].service_status is None
