"""Repair queue for the Assist service.

FIFO queue of pending repair actions. Each entry has a state machine:
  pending_confirm → applied | rejected
  applied → rolled_back (if rollback token present)

Repair actions always go through the agent's REST API, never raw systemctl.
"""

from __future__ import annotations

import secrets
import time
from collections import deque
from dataclasses import dataclass, field
from typing import Any

import structlog

log = structlog.get_logger()

CONFIRM_TIMEOUT_SECONDS = 300  # 5 minutes


@dataclass
class RepairItem:
    id: str
    proposed_at: float
    origin: str  # rule:<rule_id> | client:manual
    action: str  # tool name (e.g. services.restart)
    args: dict[str, Any]
    safety_class: str
    state: str = "pending_confirm"  # pending_confirm | applied | rolled_back | rejected
    applied_at: float | None = None
    rolled_back_at: float | None = None
    rollback_token: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "proposed_at": self.proposed_at,
            "origin": self.origin,
            "action": self.action,
            "args": self.args,
            "safety_class": self.safety_class,
            "state": self.state,
            "applied_at": self.applied_at,
            "rolled_back_at": self.rolled_back_at,
        }


class RepairQueue:
    """FIFO queue of repair actions."""

    def __init__(self) -> None:
        self._queue: deque[RepairItem] = deque(maxlen=50)

    def enqueue(
        self,
        origin: str,
        action: str,
        args: dict[str, Any],
        safety_class: str = "safe_write",
    ) -> RepairItem:
        item = RepairItem(
            id=secrets.token_hex(6),
            proposed_at=time.time(),
            origin=origin,
            action=action,
            args=args,
            safety_class=safety_class,
        )
        self._queue.append(item)
        log.info("repair_enqueued", action=action, origin=origin, id=item.id)
        return item

    def list_pending(self) -> list[RepairItem]:
        return [i for i in self._queue if i.state == "pending_confirm"]

    def list_all(self) -> list[RepairItem]:
        return list(self._queue)

    def approve(self, item_id: str) -> RepairItem | None:
        """Mark item as approved (transitions to applied after execution)."""
        item = self._get(item_id)
        if item and item.state == "pending_confirm":
            item.state = "applied"
            item.applied_at = time.time()
            log.info("repair_approved", id=item_id, action=item.action)
            return item
        return None

    def reject(self, item_id: str) -> bool:
        item = self._get(item_id)
        if item and item.state == "pending_confirm":
            item.state = "rejected"
            log.info("repair_rejected", id=item_id)
            return True
        return False

    def rollback(self, item_id: str) -> bool:
        item = self._get(item_id)
        if item and item.state == "applied":
            item.state = "rolled_back"
            item.rolled_back_at = time.time()
            log.info("repair_rolled_back", id=item_id)
            return True
        return False

    def cancel(self, item_id: str) -> bool:
        return self.reject(item_id)

    def expire_timed_out(self) -> int:
        """Auto-expire pending_confirm items past the timeout."""
        now = time.time()
        expired = 0
        for item in self._queue:
            if item.state == "pending_confirm" and (now - item.proposed_at) > CONFIRM_TIMEOUT_SECONDS:
                item.state = "rejected"
                expired += 1
        return expired

    def _get(self, item_id: str) -> RepairItem | None:
        return next((i for i in self._queue if i.id == item_id), None)
