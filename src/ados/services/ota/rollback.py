"""A/B partition boot tracking and automatic rollback."""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from enum import StrEnum
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("ota-rollback")

DEFAULT_BOOT_STATUS_PATH = "/etc/ados/boot-status.json"
MAX_BOOT_FAILURES = 3


class SlotStatus(StrEnum):
    """Partition slot states."""

    ACTIVE = "active"
    STANDBY = "standby"
    BOOTABLE = "bootable"
    UNBOOTABLE = "unbootable"


@dataclass
class BootSlot:
    """State of a single A/B partition slot."""

    slot_name: str
    version: str
    status: SlotStatus
    boot_count: int = 0
    last_boot: str = ""

    def to_dict(self) -> dict:
        result = asdict(self)
        result["status"] = self.status.value
        return result


@dataclass
class BootState:
    """Persisted boot state for both slots."""

    slot_a: BootSlot = field(
        default_factory=lambda: BootSlot(
            slot_name="a", version="0.1.0", status=SlotStatus.ACTIVE
        )
    )
    slot_b: BootSlot = field(
        default_factory=lambda: BootSlot(
            slot_name="b", version="", status=SlotStatus.STANDBY
        )
    )

    def to_dict(self) -> dict:
        return {
            "slot_a": self.slot_a.to_dict(),
            "slot_b": self.slot_b.to_dict(),
        }


class RollbackManager:
    """Manages A/B partition boot tracking and rollback decisions."""

    def __init__(self, status_path: str = DEFAULT_BOOT_STATUS_PATH) -> None:
        self._status_path = Path(status_path)
        self._state = self._load_state()

    def _load_state(self) -> BootState:
        """Load boot state from JSON file, or create defaults."""
        if not self._status_path.exists():
            log.info("boot_status_not_found, creating defaults", path=str(self._status_path))
            state = BootState()
            self._save_state(state)
            return state

        try:
            data = json.loads(self._status_path.read_text())
            slot_a_data = data.get("slot_a", {})
            slot_b_data = data.get("slot_b", {})
            return BootState(
                slot_a=BootSlot(
                    slot_name=slot_a_data.get("slot_name", "a"),
                    version=slot_a_data.get("version", "0.1.0"),
                    status=SlotStatus(slot_a_data.get("status", "active")),
                    boot_count=slot_a_data.get("boot_count", 0),
                    last_boot=slot_a_data.get("last_boot", ""),
                ),
                slot_b=BootSlot(
                    slot_name=slot_b_data.get("slot_name", "b"),
                    version=slot_b_data.get("version", ""),
                    status=SlotStatus(slot_b_data.get("status", "standby")),
                    boot_count=slot_b_data.get("boot_count", 0),
                    last_boot=slot_b_data.get("last_boot", ""),
                ),
            )
        except (json.JSONDecodeError, KeyError, ValueError) as exc:
            log.warning("boot_status_corrupt", error=str(exc))
            return BootState()

    def _save_state(self, state: BootState | None = None) -> None:
        """Persist boot state to disk."""
        if state is None:
            state = self._state
        try:
            self._status_path.parent.mkdir(parents=True, exist_ok=True)
            self._status_path.write_text(json.dumps(state.to_dict(), indent=2))
        except OSError as exc:
            log.error("boot_status_save_failed", error=str(exc))

    def get_active_slot(self) -> BootSlot:
        """Return the currently active boot slot."""
        if self._state.slot_a.status == SlotStatus.ACTIVE:
            return self._state.slot_a
        return self._state.slot_b

    def get_standby_slot(self) -> BootSlot:
        """Return the standby boot slot."""
        if self._state.slot_a.status == SlotStatus.ACTIVE:
            return self._state.slot_b
        return self._state.slot_a

    def mark_boot_successful(self) -> None:
        """Reset boot counter for the active slot, confirming a good boot."""
        active = self.get_active_slot()
        active.boot_count = 0
        log.info("boot_marked_successful", slot=active.slot_name, version=active.version)
        self._save_state()

    def increment_boot_count(self) -> None:
        """Increment boot counter for the active slot. Called on startup."""
        active = self.get_active_slot()
        active.boot_count += 1
        active.last_boot = datetime.now(timezone.utc).isoformat()
        log.info(
            "boot_count_incremented",
            slot=active.slot_name,
            count=active.boot_count,
        )
        self._save_state()

    def should_rollback(self) -> bool:
        """Check if the active slot has exceeded the failure threshold."""
        active = self.get_active_slot()
        return active.boot_count >= MAX_BOOT_FAILURES

    def rollback(self) -> bool:
        """Switch the active slot to the standby slot.

        Returns True if rollback was performed, False if standby is unbootable.
        """
        standby = self.get_standby_slot()

        if standby.status == SlotStatus.UNBOOTABLE:
            log.error("rollback_failed", reason="standby_unbootable", slot=standby.slot_name)
            return False

        if not standby.version:
            log.error("rollback_failed", reason="standby_has_no_version", slot=standby.slot_name)
            return False

        active = self.get_active_slot()
        log.info(
            "performing_rollback",
            from_slot=active.slot_name,
            from_version=active.version,
            to_slot=standby.slot_name,
            to_version=standby.version,
        )

        active.status = SlotStatus.STANDBY
        active.boot_count = 0
        standby.status = SlotStatus.ACTIVE
        standby.boot_count = 0

        self._save_state()
        return True

    def prepare_standby(self, version: str) -> None:
        """Mark the standby slot as bootable with a new version.

        Called after a successful update installation to the standby partition.
        """
        standby = self.get_standby_slot()
        standby.version = version
        standby.status = SlotStatus.BOOTABLE
        standby.boot_count = 0
        log.info("standby_prepared", slot=standby.slot_name, version=version)
        self._save_state()

    def activate_standby(self) -> bool:
        """Swap active and standby slots. Called to boot into the new version.

        Returns True on success, False if standby is not bootable.
        """
        standby = self.get_standby_slot()

        if standby.status not in (SlotStatus.BOOTABLE, SlotStatus.STANDBY):
            log.error("activate_failed", reason="standby_not_bootable", status=standby.status)
            return False

        active = self.get_active_slot()
        active.status = SlotStatus.STANDBY
        standby.status = SlotStatus.ACTIVE

        self._save_state()
        log.info(
            "standby_activated",
            new_active=standby.slot_name,
            version=standby.version,
        )
        return True

    def get_status(self) -> dict:
        """Full boot status for API responses."""
        return {
            "active_slot": self.get_active_slot().to_dict(),
            "standby_slot": self.get_standby_slot().to_dict(),
            "should_rollback": self.should_rollback(),
        }
