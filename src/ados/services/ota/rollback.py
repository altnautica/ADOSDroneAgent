"""A/B partition rollback manager for OTA updates."""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field
from enum import StrEnum
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("ota-rollback")

MAX_BOOT_FAILURES = 3


class SlotStatus(StrEnum):
    ACTIVE = "active"
    STANDBY = "standby"
    BOOTABLE = "bootable"
    UNBOOTABLE = "unbootable"


@dataclass
class Slot:
    slot_name: str
    status: SlotStatus
    version: str = ""
    boot_count: int = 0


@dataclass
class _State:
    slots: list[Slot] = field(default_factory=lambda: [
        Slot(slot_name="a", status=SlotStatus.ACTIVE),
        Slot(slot_name="b", status=SlotStatus.STANDBY),
    ])


class RollbackManager:
    """Manages A/B slot state for safe OTA rollback."""

    def __init__(self, status_path: str) -> None:
        self._path = Path(status_path)
        self._state = self._load_state()

    def _load_state(self) -> _State:
        if not self._path.exists():
            return _State()
        try:
            raw = json.loads(self._path.read_text())
            slots = [
                Slot(
                    slot_name=s["slot_name"],
                    status=SlotStatus(s["status"]),
                    version=s.get("version", ""),
                    boot_count=s.get("boot_count", 0),
                )
                for s in raw.get("slots", [])
            ]
            if len(slots) == 2:
                return _State(slots=slots)
        except (json.JSONDecodeError, KeyError, ValueError):
            log.warning("corrupt_state_file", path=str(self._path))
        return _State()

    def _save_state(self) -> None:
        self._path.parent.mkdir(parents=True, exist_ok=True)
        data = {"slots": [asdict(s) for s in self._state.slots]}
        self._path.write_text(json.dumps(data))

    def _find_slot(self, status: SlotStatus) -> Slot:
        for s in self._state.slots:
            if s.status == status:
                return s
        return self._state.slots[0]

    def get_active_slot(self) -> Slot:
        return self._find_slot(SlotStatus.ACTIVE)

    def get_standby_slot(self) -> Slot:
        for s in self._state.slots:
            if s.status != SlotStatus.ACTIVE:
                return s
        return self._state.slots[1]

    def increment_boot_count(self) -> None:
        self.get_active_slot().boot_count += 1
        self._save_state()

    def mark_boot_successful(self) -> None:
        self.get_active_slot().boot_count = 0
        self._save_state()

    def should_rollback(self) -> bool:
        return self.get_active_slot().boot_count >= MAX_BOOT_FAILURES

    def prepare_standby(self, version: str) -> None:
        standby = self.get_standby_slot()
        standby.version = version
        standby.status = SlotStatus.BOOTABLE
        self._save_state()

    def rollback(self) -> bool:
        standby = self.get_standby_slot()
        if not standby.version:
            log.warning("rollback_no_version")
            return False
        if standby.status == SlotStatus.UNBOOTABLE:
            log.warning("rollback_unbootable")
            return False
        return self._swap_slots()

    def activate_standby(self) -> bool:
        standby = self.get_standby_slot()
        if not standby.version:
            return False
        return self._swap_slots()

    def _swap_slots(self) -> bool:
        active = self.get_active_slot()
        standby = self.get_standby_slot()
        active.status = SlotStatus.STANDBY
        active.boot_count = 0
        standby.status = SlotStatus.ACTIVE
        standby.boot_count = 0
        self._save_state()
        log.info(
            "slots_swapped",
            new_active=standby.slot_name,
            new_standby=active.slot_name,
        )
        return True

    def get_status(self) -> dict:
        active = self.get_active_slot()
        standby = self.get_standby_slot()
        return {
            "active_slot": asdict(active),
            "standby_slot": asdict(standby),
            "should_rollback": self.should_rollback(),
        }
