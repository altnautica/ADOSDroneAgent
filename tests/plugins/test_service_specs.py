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


def _spec(command: str) -> ServiceSpec:
    return ServiceSpec.model_validate({"name": "worker", "command": command})


def test_service_command_cannot_inject_a_unit_directive() -> None:
    """A newline in the command would start a new directive; an
    ``ExecStartPre=+`` runs as root outside the sandbox."""
    from ados.plugins.systemd import render_service_unit

    manifest = _service_manifest()
    for command in (
        "/bin/worker\nExecStartPre=+/bin/sh -c id",
        "/bin/worker\rUser=root",
        "+/bin/sh -c id",
        "!/bin/worker",
        "/bin/worker ; /bin/sh",
        "'unbalanced",
    ):
        with pytest.raises(ValueError):
            render_service_unit(manifest, _spec(command), Path("/var/ados/plugins"))


def test_service_command_words_are_quoted_for_systemd() -> None:
    from ados.plugins.systemd import exec_start_value

    assert exec_start_value("/bin/worker --flag") == "/bin/worker --flag"
    # Spaces stay inside one argument; specifiers and variables never expand.
    assert (
        exec_start_value("/bin/worker 'two words' 100% $HOME")
        == '/bin/worker "two words" "100%%" "$$HOME"'
    )
    assert exec_start_value('/bin/worker "a\\"b"') == '/bin/worker "a\\"b"'


def test_service_slice_is_always_the_plugin_slice() -> None:
    from ados.plugins.errors import ManifestError
    from ados.plugins.systemd import render_service_unit

    with pytest.raises(ManifestError):
        ServiceSpec.model_validate(
            {"name": "worker", "command": "/bin/worker", "slice": "system.slice"}
        )
    # Even a spec built without validation renders into the plugin slice.
    spec = ServiceSpec.model_construct(
        name="worker", command="/bin/worker", slice="system.slice"
    )
    unit = render_service_unit(_service_manifest(), spec, Path("/var/ados/plugins"))
    assert "Slice=ados-plugins.slice" in unit
    assert "system.slice" not in unit


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


def _probe(sup: PluginSupervisor, ready_check: str, returncode: int = 0):
    """Run one command probe with subprocess stubbed; return the verdict and
    the (args, kwargs) the probe handed to ``subprocess.run``."""
    spec = ServiceSpec.model_validate(
        {"name": "worker", "command": "noop", "ready_check": ready_check}
    )
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(
            returncode=returncode, stderr="not yet", stdout=""
        )
        verdict = sup._probe_service_ready(
            "com.example.daemonplug", _service_manifest(), spec
        )
    assert run_mock.call_count == 1
    return verdict, run_mock.call_args


def test_command_ready_check_runs_as_a_sandboxed_argv(isolated_paths):
    """A non-URL ready_check is an argv run by systemd-run as the plugin user
    inside the plugin's sandbox, never a shell string in the API process."""
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    (ready, reason), call = _probe(sup, "/usr/bin/test -S 'run/worker sock'")
    assert (ready, reason) == (True, None)

    argv = call.args[0]
    assert isinstance(argv, list)
    assert call.kwargs.get("shell") is not True
    assert argv[0] == "systemd-run"
    assert "--uid=ados" in argv and "--gid=ados" in argv
    assert "--slice=ados-plugins.slice" in argv
    # The plugin's own sandbox: hardening, its resource envelope, and the
    # capability sandbox that hides the agent's command sockets.
    assert "--property=NoNewPrivileges=yes" in argv
    assert "--property=MemoryMax=48M" in argv
    assert any(
        a.startswith("--property=InaccessiblePaths=") and "-/run/ados/control.sock" in a
        for a in argv
    )
    # The probe's own words come after `--`, one argv element each.
    sep = argv.index("--")
    assert argv[sep + 1 :] == ["/usr/bin/test", "-S", "run/worker sock"]


def test_shell_syntax_in_a_ready_check_is_never_interpreted(isolated_paths):
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    _, call = _probe(sup, "cp /bin/sh /tmp/s; chmod 4755 /tmp/s")
    argv = call.args[0]
    sep = argv.index("--")
    # `;` stays a literal argument to `cp`; no second command exists.
    assert argv[sep + 1 :] == ["cp", "/bin/sh", "/tmp/s;", "chmod", "4755", "/tmp/s"]
    assert call.kwargs.get("shell") is not True


def test_command_ready_check_nonzero_is_not_ready(isolated_paths):
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    (ready, reason), _ = _probe(sup, "/usr/bin/false", returncode=7)
    assert ready is False
    assert reason == "exit 7: not yet"


@pytest.mark.parametrize(
    "ready_check",
    [
        "/bin/true\nExecStartPre=+/bin/sh",
        "/bin/true\x00",
        "http://192.168.1.50:9100/healthz",
        "http://127.0.0.1/healthz",
        "https://user:pw@127.0.0.1:9100/",
        "   ",
        "'unterminated",
    ],
)
def test_manifest_rejects_an_unsafe_ready_check(ready_check: str) -> None:
    from ados.plugins.errors import ManifestError

    with pytest.raises(ManifestError):
        ServiceSpec.model_validate(
            {"name": "worker", "command": "noop", "ready_check": ready_check}
        )


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
