"""Plugin systemd unit rendering tests."""

from __future__ import annotations

import pytest

from ados.plugins.manifest import PluginManifest
from ados.plugins.systemd import (
    PLUGIN_SLICE_NAME,
    render_unit,
    slice_unit_content,
    unit_name_for,
    unit_path_for,
)


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


def test_render_unit_emits_resource_limits() -> None:
    unit = render_unit(_subprocess_manifest())
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


def test_render_unit_rejects_inprocess() -> None:
    with pytest.raises(ValueError):
        render_unit(_inprocess_manifest())


def test_slice_content_has_accounting_directives() -> None:
    content = slice_unit_content()
    assert "[Slice]" in content
    assert "CPUAccounting=yes" in content
    assert "MemoryAccounting=yes" in content
    assert "TasksAccounting=yes" in content
