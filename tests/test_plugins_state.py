"""Plugin state persistence tests."""

from __future__ import annotations

from pathlib import Path

from ados.plugins.state import (
    PermissionGrant,
    PluginInstall,
    find_install,
    grant_permission,
    is_permission_granted,
    load_state,
    remove_install,
    revoke_permission,
    save_state,
    upsert_install,
)


def _basic_install(plugin_id: str = "com.example.basic") -> PluginInstall:
    return PluginInstall(
        plugin_id=plugin_id,
        version="0.1.0",
        source="local_file",
        source_uri="/tmp/basic.adosplug",
        signer_id="altnautica-test",
        manifest_hash="0" * 64,
        status="installed",
        installed_at=1735000000,
    )


def test_save_and_load_round_trip(tmp_path: Path) -> None:
    p = tmp_path / "plugin-state.json"
    install = _basic_install()
    install.permissions = {
        "hardware.spi": PermissionGrant(granted=True, granted_at=1735001000),
        "vehicle.command": PermissionGrant(granted=False, granted_at=None),
    }
    save_state([install], p)
    out = load_state(p)
    assert len(out) == 1
    assert out[0].plugin_id == "com.example.basic"
    assert out[0].permissions["hardware.spi"].granted is True
    assert out[0].permissions["vehicle.command"].granted is False


def test_load_missing_file_returns_empty(tmp_path: Path) -> None:
    assert load_state(tmp_path / "absent.json") == []


def test_upsert_replaces_same_id(tmp_path: Path) -> None:
    inst1 = _basic_install()
    inst2 = _basic_install()
    inst2.version = "0.2.0"
    out = upsert_install([inst1], inst2)
    assert len(out) == 1
    assert out[0].version == "0.2.0"


def test_upsert_appends_new_id() -> None:
    a = _basic_install("com.example.a")
    b = _basic_install("com.example.b")
    out = upsert_install([a], b)
    assert len(out) == 2


def test_remove_install() -> None:
    a = _basic_install("com.example.a")
    b = _basic_install("com.example.b")
    out = remove_install([a, b], "com.example.a")
    assert len(out) == 1
    assert out[0].plugin_id == "com.example.b"


def test_grant_and_revoke() -> None:
    inst = _basic_install()
    grant_permission(inst, "hardware.spi")
    assert is_permission_granted(inst, "hardware.spi")
    revoke_permission(inst, "hardware.spi")
    assert not is_permission_granted(inst, "hardware.spi")
    assert inst.permissions["hardware.spi"].revoked_at is not None


def test_find_install_returns_none_when_absent() -> None:
    assert find_install([], "com.example.absent") is None


def test_corrupt_state_file_returns_empty(tmp_path: Path) -> None:
    p = tmp_path / "corrupt.json"
    p.write_text("{not valid json", encoding="utf-8")
    assert load_state(p) == []
