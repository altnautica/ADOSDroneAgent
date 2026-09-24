"""Security regression tests for the plugin module.

Each test pins one finding from the security review so it cannot
regress (symlink injection, entrypoint traversal). The signer
allowlist, grant filtering and manifest-hash checks live in the native
plugin host.
"""

from __future__ import annotations

import io
import zipfile

import pytest
import yaml

from ados.plugins.archive import (
    MANIFEST_FILENAME,
    SYMLINK_MODE,
    _safe_member_path,
    parse_archive_bytes,
)
from ados.plugins.errors import ArchiveError, ManifestError
from ados.plugins.manifest import PluginManifest


def _basic_manifest_yaml() -> str:
    return """\
schema_version: 1
id: com.example.basic
version: 0.1.0
name: Basic
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=0.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions: ["event.publish"]
"""


# -----------------------------------------------------------------
# Symlink rejection (security finding #1, CRITICAL)
# -----------------------------------------------------------------


def test_symlink_in_archive_rejected() -> None:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as zf:
        zf.writestr(MANIFEST_FILENAME, _basic_manifest_yaml())
        info = zipfile.ZipInfo("agent/evil.py")
        info.external_attr = (SYMLINK_MODE | 0o777) << 16
        zf.writestr(info, "../../../etc/ados/config.yaml")
    with pytest.raises(ArchiveError, match="symlink"):
        parse_archive_bytes(buf.getvalue())


def test_double_dot_in_path_segment_rejected() -> None:
    with pytest.raises(ArchiveError):
        _safe_member_path("agent/..hidden/plugin.py")


def test_backslash_path_rejected() -> None:
    with pytest.raises(ArchiveError):
        _safe_member_path("agent\\plugin.py")


# -----------------------------------------------------------------
# Entrypoint path traversal (security finding #2, HIGH)
# -----------------------------------------------------------------


def _good() -> dict:
    return yaml.safe_load(_basic_manifest_yaml())


def test_entrypoint_must_be_relative() -> None:
    bad = _good()
    bad["agent"]["entrypoint"] = "/opt/ados/evil.py"
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(yaml.safe_dump(bad))


def test_entrypoint_no_dot_dot() -> None:
    bad = _good()
    bad["agent"]["entrypoint"] = "../etc/ados/config.yaml"
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(yaml.safe_dump(bad))


def test_entrypoint_no_dot_dot_inside_path() -> None:
    bad = _good()
    bad["agent"]["entrypoint"] = "agent/../../escape.py"
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(yaml.safe_dump(bad))


def test_entrypoint_module_id_form_allowed() -> None:
    good = _good()
    good["agent"]["entrypoint"] = "ados_geofence:GeofencePlugin"
    m = PluginManifest.from_yaml_text(yaml.safe_dump(good))
    assert m.agent.entrypoint == "ados_geofence:GeofencePlugin"


def test_entrypoint_empty_rejected() -> None:
    bad = _good()
    bad["agent"]["entrypoint"] = ""
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(yaml.safe_dump(bad))


@pytest.mark.parametrize(
    "entrypoint",
    [
        "bin/x\nExecStartPre=+/bin/sh -c id",
        "bin/x\rExecStartPre=+/bin/sh",
        "bin/x\x1b[2J",
        "pkg.mod:Class\nExecStartPre=+/bin/sh",
    ],
)
def test_entrypoint_with_a_control_character_rejected(entrypoint: str) -> None:
    """An entrypoint lands in a generated unit file, where a newline would
    start a new directive; neither the path nor the module:Class form may
    carry one."""
    bad = _good()
    bad["agent"]["entrypoint"] = entrypoint
    with pytest.raises(ManifestError, match="entrypoint"):
        PluginManifest.from_yaml_text(yaml.safe_dump(bad))


@pytest.mark.parametrize(
    "entrypoint",
    [
        "agent/bin/p --flag",
        "agent/bin/p%h",
        "agent/bin/$HOME",
        "agent//bin/p",
        "pkg-mod:Class",
        "pkg.mod:Class()",
    ],
)
def test_entrypoint_outside_the_unit_safe_grammar_rejected(entrypoint: str) -> None:
    """A rust entrypoint is interpolated into ExecStart, where a space adds an
    argv word and ``%``/``$`` expand; the LAN validator must refuse what the
    cloud-relay (Rust) parser refuses."""
    bad = _good()
    bad["agent"]["entrypoint"] = entrypoint
    with pytest.raises(ManifestError, match="entrypoint"):
        PluginManifest.from_yaml_text(yaml.safe_dump(bad))
