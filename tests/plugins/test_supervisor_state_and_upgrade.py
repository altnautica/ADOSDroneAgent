"""PluginSupervisor against a shared state file and over an existing install.

Several processes write ``plugin-state.json`` (the REST API, the CLI, the
cloud-relay path), so a supervisor must never save a stale copy of it. And an
install over an existing plugin must neither leave the old process running nor
destroy the working install when the new archive turns out to be bad.
"""

from __future__ import annotations

import json
import zipfile
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from ados.plugins.archive import MANIFEST_FILENAME
from ados.plugins.errors import ArchiveError
from ados.plugins.supervisor import PluginSupervisor

SERVICE_ID = "com.example.daemonplug"


def _archive(
    tmp_path: Path,
    plugin_id: str,
    *,
    version: str = "0.1.0",
    permissions: str = '["event.publish"]',
    services: str = "",
    gcs: bool = False,
    ship_gcs: bool = True,
) -> Path:
    manifest = f"""\
id: {plugin_id}
version: {version}
name: Test
compatibility:
  ados_version: ">=0.0.0"
agent:
  entrypoint: agent/plugin.py
  permissions: {permissions}
{services}"""
    if gcs:
        manifest += "gcs:\n  entrypoint: gcs/plugin.bundle.js\n"
    path = tmp_path / f"{plugin_id}-{version}.adosplug"
    with zipfile.ZipFile(path, "w") as zf:
        zf.writestr(MANIFEST_FILENAME, manifest)
        zf.writestr("agent/plugin.py", f"# {version}\n")
        if gcs and ship_gcs:
            zf.writestr("gcs/plugin.bundle.js", "export {};\n")
    return path


@pytest.fixture
def paths(tmp_path: Path, monkeypatch):
    state_path = tmp_path / "state" / "plugin-state.json"
    state_path.parent.mkdir()
    unit_dir = tmp_path / "systemd"
    unit_dir.mkdir()
    monkeypatch.setattr("ados.plugins.state.PLUGIN_STATE_PATH", state_path)
    monkeypatch.setattr("ados.plugins.signing.PLUGIN_KEYS_DIR", tmp_path / "keys")
    monkeypatch.setattr("ados.plugins.systemd.PLUGIN_UNIT_DIR", unit_dir)
    monkeypatch.setattr("ados.plugins.systemd.PLUGIN_LOG_DIR", tmp_path / "log")
    from ados.plugins import systemd as systemd_mod

    monkeypatch.setattr(
        systemd_mod, "PLUGIN_SLICE_PATH", unit_dir / systemd_mod.PLUGIN_SLICE_NAME
    )
    return {
        "install_dir": tmp_path / "plugins",
        "state_path": state_path,
        "unit_dir": unit_dir,
    }


def _supervisor(paths) -> PluginSupervisor:
    sup = PluginSupervisor(install_dir=paths["install_dir"], require_signed=False)
    sup.discover()
    return sup


@pytest.fixture
def systemctl():
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="", stdout="")
        yield run_mock


def _state_ids(paths) -> set[str]:
    raw = json.loads(paths["state_path"].read_text())
    return {entry["plugin_id"] for entry in raw["installs"]}


def test_a_mutation_keeps_installs_another_process_wrote(paths, tmp_path, systemctl):
    """The long-lived API supervisor must not erase a cloud/CLI install."""
    api = _supervisor(paths)
    api.install_archive(_archive(tmp_path, "com.example.lan"))

    other = _supervisor(paths)  # e.g. the CLI or the cloud-relay path
    other.install_archive(_archive(tmp_path, "com.example.cloud"))

    assert {i.plugin_id for i in api.installs()} == {
        "com.example.lan",
        "com.example.cloud",
    }
    api.grant_permission("com.example.lan", "event.publish")
    assert _state_ids(paths) == {"com.example.lan", "com.example.cloud"}


def test_a_grant_reaches_the_declared_service_unit(paths, tmp_path, systemctl):
    """A declared daemon runs in the same sandbox as the main unit."""
    services = (
        "  contributes:\n"
        "    services:\n"
        "      - name: worker\n"
        "        command: /bin/true\n"
    )
    sup = _supervisor(paths)
    sup.install_archive(
        _archive(
            tmp_path,
            SERVICE_ID,
            permissions='["hardware.uart"]',
            services=services,
        )
    )
    sup.enable(SERVICE_ID)
    svc_unit = paths["unit_dir"] / "ados-plugin-com-example-daemonplug-worker.service"
    assert "PrivateDevices=yes" in svc_unit.read_text()

    sup.grant_permission(SERVICE_ID, "hardware.uart")

    text = svc_unit.read_text()
    assert "DeviceAllow=char-ttyUSB rw" in text
    assert "PrivateDevices=yes" not in text
    restarted = [
        c.args[0]
        for c in systemctl.call_args_list
        if c.args[0][:2] == ["systemctl", "restart"]
    ]
    assert ["systemctl", "restart", svc_unit.name] in restarted


def test_reinstalling_a_running_plugin_stops_it_before_the_swap(
    paths, tmp_path, systemctl
):
    sup = _supervisor(paths)
    sup.install_archive(_archive(tmp_path, "com.example.up"))
    sup.enable("com.example.up")
    systemctl.reset_mock()

    sup.install_archive(_archive(tmp_path, "com.example.up", version="0.2.0"))

    calls = [c.args[0] for c in systemctl.call_args_list]
    assert ["systemctl", "stop", "ados-plugin-com-example-up.service"] in calls
    assert sup.find_install("com.example.up").status == "installed"


def test_a_bad_upgrade_leaves_the_working_install_intact(paths, tmp_path, systemctl):
    sup = _supervisor(paths)
    sup.install_archive(_archive(tmp_path, "com.example.keep", gcs=True))
    plugin_dir = paths["install_dir"] / "com.example.keep"

    broken = _archive(
        tmp_path, "com.example.keep", version="0.2.0", gcs=True, ship_gcs=False
    )
    with pytest.raises(ArchiveError):
        sup.install_archive(broken)

    assert (plugin_dir / "agent" / "plugin.py").read_text() == "# 0.1.0\n"
    assert (plugin_dir / "gcs" / "plugin.bundle.js").exists()
    # The recorded manifest hash still matches the files on disk.
    assert sup.manifest_for("com.example.keep").version == "0.1.0"
    assert not list(paths["install_dir"].glob(".*"))


def test_a_builtin_installs_as_a_subprocess_plugin_the_runner_can_load(
    paths, systemctl
):
    """Built-ins run like any other plugin: a unit the host serves and the
    shared runner executes, with a manifest on disk it can load."""
    from ados.plugins.builtin.geofence import PLUGIN_ID, GeofencePlugin
    from ados.plugins.manifest import PluginManifest
    from ados.plugins.runner import _load_plugin_class

    sup = _supervisor(paths)
    result = sup.install_builtin(PLUGIN_ID)

    assert result.plugin_id == PLUGIN_ID
    plugin_dir = paths["install_dir"] / PLUGIN_ID
    manifest = PluginManifest.from_yaml_file(plugin_dir / MANIFEST_FILENAME)
    assert manifest.agent.isolation == "subprocess"
    unit = paths["unit_dir"] / "ados-plugin-io-altnautica-geofence.service"
    assert f"ados-plugin-runner {PLUGIN_ID}" in unit.read_text()
    assert _load_plugin_class(plugin_dir, manifest) is GeofencePlugin

    sup.grant_permission(PLUGIN_ID, "event.subscribe")
    sup.enable(PLUGIN_ID)
    assert sup.find_install(PLUGIN_ID).status == "running"
    calls = [c.args[0] for c in systemctl.call_args_list]
    assert ["systemctl", "start", unit.name] in calls


def test_an_inprocess_agent_half_is_refused(paths, tmp_path, systemctl):
    """No process executes an in-process agent half, so installing one would
    record a plugin that reads enabled and never runs."""
    from ados.plugins.errors import SupervisorError

    text = zipfile.ZipFile(_archive(tmp_path, "com.example.inproc")).read(
        MANIFEST_FILENAME
    ).decode()
    text = text.replace(
        "  entrypoint: agent/plugin.py\n",
        "  entrypoint: pkg.mod:Plugin\n  isolation: inprocess\n",
    )
    rebuilt = tmp_path / "inproc.adosplug"
    with zipfile.ZipFile(rebuilt, "w") as zf:
        zf.writestr(MANIFEST_FILENAME, text)
    with pytest.raises(SupervisorError, match="inprocess"):
        _supervisor(paths).install_archive(rebuilt)
