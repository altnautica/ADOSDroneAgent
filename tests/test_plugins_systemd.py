"""Plugin systemd unit rendering tests."""

from __future__ import annotations

from pathlib import Path

import pytest

from ados.core.paths import PLUGIN_RUN_DIR
from ados.plugins.manifest import PluginManifest
from ados.plugins.systemd import (
    PLUGIN_SLICE_NAME,
    render_unit,
    slice_unit_content,
    unit_name_for,
    unit_path_for,
)

_INSTALL_DIR = Path("/var/ados/plugins")


def _subprocess_manifest() -> PluginManifest:
    return PluginManifest.from_yaml_text(
        """\
schema_version: 1
id: com.example.thermal-lepton
version: 0.2.1
name: Thermal Lepton
license: GPL-3.0-or-later
risk: medium
compatibility:
  ados_version: ">=0.9.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions: ["hardware.spi", "event.publish"]
  resources:
    max_ram_mb: 80
    max_cpu_percent: 30
    max_pids: 16
"""
    )


def _inprocess_manifest() -> PluginManifest:
    return PluginManifest.from_yaml_text(
        """\
schema_version: 1
id: com.altnautica.geofence
version: 0.4.2
name: Geofence
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=0.9.0"
agent:
  entrypoint: ados_geofence:GeofencePlugin
  isolation: inprocess
  permissions: ["mavlink.read", "event.publish"]
"""
    )


def test_unit_name_replaces_dots_with_hyphens() -> None:
    assert (
        unit_name_for("com.example.thermal-lepton")
        == "ados-plugin-com-example-thermal-lepton.service"
    )


def test_unit_path_lives_in_systemd_dir() -> None:
    p = unit_path_for("com.example.thermal-lepton")
    assert str(p).startswith("/etc/systemd/system/")
    assert str(p).endswith(".service")


def _rust_manifest() -> PluginManifest:
    return PluginManifest.from_yaml_text(
        """\
schema_version: 1
id: com.example.rustplug
version: 1.0.0
name: Rust Plug
license: GPL-3.0-or-later
risk: medium
compatibility:
  ados_version: ">=0.9.0"
agent:
  entrypoint: agent/bin/com.example.rustplug
  runtime: rust
  resources:
    max_ram_mb: 64
    max_cpu_percent: 30
    max_pids: 8
"""
    )


def test_render_unit_emits_resource_limits() -> None:
    unit = render_unit(_subprocess_manifest(), _INSTALL_DIR)
    assert "MemoryMax=80M" in unit
    assert "CPUQuota=30%" in unit
    assert "TasksMax=16" in unit
    assert f"Slice={PLUGIN_SLICE_NAME}" in unit
    assert "Restart=on-failure" in unit
    assert "StartLimitBurst=5" in unit
    assert "StartLimitInterval=60s" in unit
    assert "PrivateTmp=yes" in unit
    assert "ProtectSystem=strict" in unit
    assert "NoNewPrivileges=yes" in unit
    # Python runtime (default): the shared runner takes the plugin id.
    assert (
        "ExecStart=/opt/ados/venv/bin/ados-plugin-runner "
        "com.example.thermal-lepton" in unit
    )


def test_render_unit_rust_runtime_execs_the_plugin_binary() -> None:
    unit = render_unit(_rust_manifest(), _INSTALL_DIR)
    # ExecStart points at the unpacked plugin binary with its socket path.
    sock = PLUGIN_RUN_DIR / "com.example.rustplug.sock"
    assert (
        f"ExecStart={_INSTALL_DIR}/com.example.rustplug/"
        "agent/bin/com.example.rustplug "
        "com.example.rustplug "
        f"--socket {sock}" in unit
    )
    # The token is never on the ExecStart line (it comes from the unit env).
    exec_line = next(
        line for line in unit.splitlines() if line.startswith("ExecStart=")
    )
    assert "token" not in exec_line.lower()
    # Shared / hardening / limit lines are identical to the python branch.
    assert f"Slice={PLUGIN_SLICE_NAME}" in unit
    assert "MemoryMax=64M" in unit
    assert "NoNewPrivileges=yes" in unit


def test_render_unit_delivers_token_via_environment_file() -> None:
    # Both runtimes deliver the capability token to the runner via a 0600
    # EnvironmentFile (the `-` prefix tolerates its absence before the first
    # mint) and expose the socket path as a static Environment line. The runner
    # reads ADOS_PLUGIN_TOKEN / ADOS_PLUGIN_SOCKET from its environment.
    for manifest in (_subprocess_manifest(), _rust_manifest()):
        unit = render_unit(manifest, _INSTALL_DIR)
        sock = PLUGIN_RUN_DIR / f"{manifest.id}.sock"
        env_file = PLUGIN_RUN_DIR / f"{manifest.id}.token.env"
        assert f"Environment=ADOS_PLUGIN_SOCKET={sock}" in unit
        assert f"EnvironmentFile=-{env_file}" in unit


def test_render_unit_rejects_inprocess() -> None:
    with pytest.raises(ValueError):
        render_unit(_inprocess_manifest(), _INSTALL_DIR)


def test_slice_content_has_accounting_directives() -> None:
    content = slice_unit_content()
    assert "[Slice]" in content
    assert "CPUAccounting=yes" in content
    assert "MemoryAccounting=yes" in content
    assert "TasksAccounting=yes" in content
