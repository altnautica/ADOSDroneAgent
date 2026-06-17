"""Plugin supervisor lifecycle tests (no real systemctl).

These tests exercise the supervisor's install / enable / disable / remove
flow against a fake systemctl (subprocess.run is monkey-patched). They
DO NOT touch real systemd. Hardware-rig tests live in the bench
verification on the bench rig.
"""

from __future__ import annotations

import zipfile
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from ados.plugins.archive import MANIFEST_FILENAME
from ados.plugins.errors import SupervisorError
from ados.plugins.supervisor import PluginSupervisor


def _build_archive(tmp_path: Path, plugin_id: str = "com.example.basic") -> Path:
    manifest_yaml = f"""\
schema_version: 1
id: {plugin_id}
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
  resources:
    max_ram_mb: 32
    max_cpu_percent: 10
    max_pids: 4
"""
    archive_path = tmp_path / f"{plugin_id}.adosplug"
    with zipfile.ZipFile(archive_path, "w") as zf:
        zf.writestr(MANIFEST_FILENAME, manifest_yaml)
        zf.writestr("agent/plugin.py", "# stub\n")
    return archive_path


@pytest.fixture
def isolated_paths(tmp_path: Path, monkeypatch):
    """Redirect every plugin path constant into ``tmp_path``."""
    install_dir = tmp_path / "var-plugins"
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
    # The slice path is computed at import time inside systemd.py; rebind it.
    from ados.plugins import systemd as systemd_mod

    monkeypatch.setattr(
        systemd_mod,
        "PLUGIN_SLICE_PATH",
        unit_dir / systemd_mod.PLUGIN_SLICE_NAME,
        raising=False,
    )
    return {
        "install_dir": install_dir,
        "state_path": state_path,
        "keys_dir": keys_dir,
        "log_dir": log_dir,
        "unit_dir": unit_dir,
    }


def test_install_unsigned_when_signing_disabled(
    isolated_paths, tmp_path: Path
):
    archive = _build_archive(tmp_path)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"],
        require_signed=False,
    )
    sup.discover()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        result = sup.install_archive(archive)
    assert result.plugin_id == "com.example.basic"
    assert result.signer_id is None
    assert (isolated_paths["install_dir"] / "com.example.basic" / MANIFEST_FILENAME).exists()
    unit_path = isolated_paths["unit_dir"] / "ados-plugin-com-example-basic.service"
    assert unit_path.exists()


def test_install_unsigned_when_signing_required_fails(
    isolated_paths, tmp_path: Path
):
    archive = _build_archive(tmp_path)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=True
    )
    sup.discover()
    from ados.plugins.errors import SignatureError

    with pytest.raises(SignatureError):
        sup.install_archive(archive)


def test_disable_then_remove(isolated_paths, tmp_path: Path):
    archive = _build_archive(tmp_path)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"],
        require_signed=False,
    )
    sup.discover()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        sup.install_archive(archive)
        sup.grant_permission("com.example.basic", "event.publish")
        sup.enable("com.example.basic")
        sup.disable("com.example.basic")
        sup.remove("com.example.basic", keep_data=True)
    assert sup.installs() == []
    unit_path = isolated_paths["unit_dir"] / "ados-plugin-com-example-basic.service"
    assert not unit_path.exists()


def test_grant_undeclared_permission_rejected(
    isolated_paths, tmp_path: Path
):
    archive = _build_archive(tmp_path)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"],
        require_signed=False,
    )
    sup.discover()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        sup.install_archive(archive)
    with pytest.raises(SupervisorError):
        sup.grant_permission("com.example.basic", "vehicle.command")


def test_remove_unknown_plugin_raises(isolated_paths):
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    sup.discover()
    with pytest.raises(SupervisorError):
        sup.remove("com.example.absent")


def test_revoke_permission_shrinks_granted_set(
    isolated_paths, tmp_path: Path
):
    archive = _build_archive(tmp_path)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"],
        require_signed=False,
    )
    sup.discover()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        sup.install_archive(archive)
        sup.grant_permission("com.example.basic", "event.publish")
    install = sup.find_install("com.example.basic")
    assert install is not None
    assert install.permissions["event.publish"].granted is True
    sup.revoke_permission("com.example.basic", "event.publish")
    assert install.permissions["event.publish"].granted is False


def test_revoke_permission_unknown_plugin_raises(isolated_paths):
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    sup.discover()
    with pytest.raises(SupervisorError):
        sup.revoke_permission("com.example.absent", "event.publish")


def test_revoke_permission_unknown_id_is_noop(isolated_paths, tmp_path: Path):
    archive = _build_archive(tmp_path)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"],
        require_signed=False,
    )
    sup.discover()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        sup.install_archive(archive)
    # Permission was never granted; revoking is silently a no-op.
    sup.revoke_permission("com.example.basic", "vehicle.command")
    install = sup.find_install("com.example.basic")
    assert install is not None
    assert "vehicle.command" not in install.permissions


def test_compatibility_blocks_install_when_constraint_excludes(
    isolated_paths, tmp_path: Path, monkeypatch
):
    archive_path = tmp_path / "future.adosplug"
    with zipfile.ZipFile(archive_path, "w") as zf:
        zf.writestr(
            MANIFEST_FILENAME,
            """\
schema_version: 1
id: com.example.future
version: 0.1.0
name: Future
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=99.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions: []
""",
        )
        zf.writestr("agent/plugin.py", "# stub\n")
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    sup.discover()
    with pytest.raises(SupervisorError):
        sup.install_archive(archive_path)


def _build_min_tier_archive(
    tmp_path: Path, min_tier: int, plugin_id: str = "com.example.tiered"
) -> Path:
    """Build a plugin archive whose compatibility declares a min_tier floor."""
    manifest_yaml = f"""\
schema_version: 1
id: {plugin_id}
version: 0.1.0
name: Tiered
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=0.0.0"
  min_tier: {min_tier}
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions: []
"""
    archive_path = tmp_path / f"{plugin_id}.adosplug"
    with zipfile.ZipFile(archive_path, "w") as zf:
        zf.writestr(MANIFEST_FILENAME, manifest_yaml)
        zf.writestr("agent/plugin.py", "# stub\n")
    return archive_path


def test_min_tier_blocks_install_on_under_tier_board(
    isolated_paths, tmp_path: Path
):
    """A board below the plugin's min_tier floor is refused with a reason."""
    archive = _build_min_tier_archive(tmp_path, min_tier=4)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"],
        require_signed=False,
        current_board_tier=2,
    )
    sup.discover()
    with pytest.raises(SupervisorError, match="requires compute tier 4"):
        sup.install_archive(archive)


def test_min_tier_allows_install_at_or_above_tier(
    isolated_paths, tmp_path: Path
):
    """A board at or above the floor installs normally."""
    archive = _build_min_tier_archive(tmp_path, min_tier=2)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"],
        require_signed=False,
        current_board_tier=3,
    )
    sup.discover()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        result = sup.install_archive(archive)
    assert result.plugin_id == "com.example.tiered"


def test_absent_min_tier_installs_as_before(isolated_paths, tmp_path: Path):
    """No min_tier means no floor: install succeeds even on a low tier."""
    archive = _build_archive(tmp_path)  # baseline manifest, no min_tier
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"],
        require_signed=False,
        current_board_tier=1,
    )
    sup.discover()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        result = sup.install_archive(archive)
    assert result.plugin_id == "com.example.basic"


def test_min_tier_lenient_when_board_tier_unknown(
    isolated_paths, tmp_path: Path
):
    """An unknown board tier never blocks, even with a high floor declared."""
    archive = _build_min_tier_archive(tmp_path, min_tier=4)
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"],
        require_signed=False,
        current_board_tier=None,
    )
    sup.discover()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        result = sup.install_archive(archive)
    assert result.plugin_id == "com.example.tiered"
