"""Tests for OTA A/B partition rollback manager."""

from __future__ import annotations

import pytest
from ados.services.ota.rollback import (
    RollbackManager,
    SlotStatus,
)


@pytest.fixture
def manager(tmp_path):
    return RollbackManager(status_path=str(tmp_path / "boot-status.json"))


def test_default_state(manager):
    active = manager.get_active_slot()
    assert active.slot_name == "a"
    assert active.status == SlotStatus.ACTIVE

    standby = manager.get_standby_slot()
    assert standby.slot_name == "b"
    assert standby.status == SlotStatus.STANDBY


def test_increment_boot_count(manager):
    manager.increment_boot_count()
    assert manager.get_active_slot().boot_count == 1

    manager.increment_boot_count()
    assert manager.get_active_slot().boot_count == 2


def test_mark_boot_successful(manager):
    manager.increment_boot_count()
    manager.increment_boot_count()
    assert manager.get_active_slot().boot_count == 2

    manager.mark_boot_successful()
    assert manager.get_active_slot().boot_count == 0


def test_should_rollback(manager):
    assert manager.should_rollback() is False

    for _ in range(3):
        manager.increment_boot_count()

    assert manager.should_rollback() is True


def test_rollback_swaps_slots(manager):
    # Prepare standby with a version
    manager.prepare_standby("0.2.0")
    standby = manager.get_standby_slot()
    assert standby.version == "0.2.0"

    # Rollback
    success = manager.rollback()
    assert success is True

    # Now B is active, A is standby
    active = manager.get_active_slot()
    assert active.slot_name == "b"
    assert active.version == "0.2.0"


def test_rollback_fails_no_version(manager):
    # Standby has no version
    success = manager.rollback()
    assert success is False


def test_rollback_fails_unbootable(manager):
    standby = manager.get_standby_slot()
    standby.status = SlotStatus.UNBOOTABLE
    manager._save_state()

    success = manager.rollback()
    assert success is False


def test_prepare_standby(manager):
    manager.prepare_standby("1.0.0")
    standby = manager.get_standby_slot()
    assert standby.version == "1.0.0"
    assert standby.status == SlotStatus.BOOTABLE


def test_activate_standby(manager):
    manager.prepare_standby("0.3.0")
    success = manager.activate_standby()
    assert success is True

    active = manager.get_active_slot()
    assert active.slot_name == "b"
    assert active.version == "0.3.0"


def test_state_persists(tmp_path):
    path = str(tmp_path / "boot-status.json")
    m1 = RollbackManager(status_path=path)
    m1.increment_boot_count()
    m1.prepare_standby("0.5.0")

    m2 = RollbackManager(status_path=path)
    assert m2.get_active_slot().boot_count == 1
    assert m2.get_standby_slot().version == "0.5.0"


def test_corrupt_state_file(tmp_path):
    path = tmp_path / "boot-status.json"
    path.write_text("not json!")

    manager = RollbackManager(status_path=str(path))
    # Should fall back to defaults
    assert manager.get_active_slot().slot_name == "a"


def test_get_status(manager):
    status = manager.get_status()
    assert "active_slot" in status
    assert "standby_slot" in status
    assert "should_rollback" in status
    assert status["should_rollback"] is False
