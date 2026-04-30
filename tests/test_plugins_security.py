"""Security regression tests for the plugin module.

Each test pins one finding from the security review so it cannot
regress (symlink injection, entrypoint traversal, signer-prefix
collision, state-permission tampering, manifest-hash drift).
"""

from __future__ import annotations

import io
import zipfile
from pathlib import Path

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
from ados.plugins.signing import (
    FIRST_PARTY_SIGNERS,
    is_first_party_signer,
)
from ados.plugins.state import (
    PermissionGrant,
    PluginInstall,
    filter_permissions_against_manifest,
)


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


# -----------------------------------------------------------------
# First-party signer allowlist (security finding #6, MEDIUM)
# -----------------------------------------------------------------


def test_first_party_allowlist_strict() -> None:
    # Currently-allowed first-party ids
    assert is_first_party_signer("altnautica-2026-A")
    assert is_first_party_signer("altnautica-2026-B")
    # An impostor that would have passed the old prefix check
    assert not is_first_party_signer("altnautica-malicious")
    assert not is_first_party_signer("altnautica-")
    assert not is_first_party_signer("altnautica-2025-X")
    assert not is_first_party_signer("ALTNAUTICA-2026-A")


def test_first_party_allowlist_constants_immutable() -> None:
    # frozenset is the correct shape; keep that.
    assert isinstance(FIRST_PARTY_SIGNERS, frozenset)


# -----------------------------------------------------------------
# State permissions filtering (security finding #5, MEDIUM)
# -----------------------------------------------------------------


def test_filter_drops_undeclared_permissions() -> None:
    install = PluginInstall(
        plugin_id="com.example.basic",
        version="0.1.0",
        source="local_file",
        source_uri=None,
        signer_id=None,
        manifest_hash="0" * 64,
        status="installed",
        installed_at=0,
        permissions={
            "event.publish": PermissionGrant(granted=True, granted_at=1),
            "vehicle.command": PermissionGrant(granted=True, granted_at=2),
        },
    )
    declared = {"event.publish"}
    filter_permissions_against_manifest(install, declared)
    assert "vehicle.command" not in install.permissions
    assert install.permissions["event.publish"].granted is True


# -----------------------------------------------------------------
# Manifest hash tamper detection (code review finding #11, LOW)
# -----------------------------------------------------------------


def test_manifest_hash_check_blocks_tampered_load(
    tmp_path: Path, monkeypatch
) -> None:
    """Smoke test: supervisor._manifest_for raises if disk hash drifts."""
    import hashlib

    import ados.plugins.systemd as _systemd
    from ados.plugins.state import PluginInstall
    from ados.plugins.supervisor import PluginSupervisor

    install_dir = tmp_path / "installs"
    plugin_dir = install_dir / "com.example.basic"
    plugin_dir.mkdir(parents=True)
    manifest_text = _basic_manifest_yaml()
    (plugin_dir / MANIFEST_FILENAME).write_text(manifest_text)

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
    monkeypatch.setattr(_systemd, "PLUGIN_UNIT_DIR", unit_dir, raising=False)
    monkeypatch.setattr(_systemd, "PLUGIN_LOG_DIR", log_dir, raising=False)
    monkeypatch.setattr(
        _systemd,
        "PLUGIN_SLICE_PATH",
        unit_dir / _systemd.PLUGIN_SLICE_NAME,
        raising=False,
    )

    sup = PluginSupervisor(install_dir=install_dir, require_signed=False)
    expected_hash = hashlib.sha256(manifest_text.encode()).hexdigest()
    sup._installs = [
        PluginInstall(
            plugin_id="com.example.basic",
            version="0.1.0",
            source="local_file",
            source_uri=None,
            signer_id=None,
            manifest_hash=expected_hash,
            status="installed",
            installed_at=0,
        )
    ]

    # Hash matches: load succeeds.
    m = sup._manifest_for("com.example.basic")
    assert m.id == "com.example.basic"

    # Tamper: rewrite manifest to a different (but valid) version on disk.
    tampered = manifest_text.replace("0.1.0", "9.9.9", 1)
    (plugin_dir / MANIFEST_FILENAME).write_text(tampered)
    from ados.plugins.errors import SupervisorError

    with pytest.raises(SupervisorError, match="manifest hash mismatch"):
        sup._manifest_for("com.example.basic")
