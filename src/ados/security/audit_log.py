"""Append-only security event audit log."""

from __future__ import annotations

import json
import shutil
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("audit-log")

DEFAULT_LOG_PATH = "/var/ados/audit.jsonl"
MAX_LOG_SIZE_BYTES = 10 * 1024 * 1024  # 10 MB


@dataclass
class AuditEvent:
    """A single security audit event."""

    timestamp: str
    event_type: str
    source_ip: str
    details: str
    success: bool

    def to_json(self) -> str:
        return json.dumps(asdict(self), separators=(",", ":"))

    @classmethod
    def create(
        cls,
        event_type: str,
        source_ip: str = "",
        details: str = "",
        success: bool = True,
    ) -> AuditEvent:
        """Create an event with the current UTC timestamp."""
        return cls(
            timestamp=datetime.now(timezone.utc).isoformat(),
            event_type=event_type,
            source_ip=source_ip,
            details=details,
            success=success,
        )


class AuditLogger:
    """Append-only JSON Lines audit log for security events.

    Each event is written as a single JSON line. The log file is
    rotated when it exceeds MAX_LOG_SIZE_BYTES. Existing entries
    are never modified or deleted (only rotated to .old).
    """

    def __init__(self, log_path: str = DEFAULT_LOG_PATH) -> None:
        self._log_path = Path(log_path)

    @property
    def path(self) -> str:
        return str(self._log_path)

    def _ensure_dir(self) -> bool:
        """Ensure the log directory exists."""
        try:
            self._log_path.parent.mkdir(parents=True, exist_ok=True)
            return True
        except OSError as exc:
            log.warning("audit_dir_create_failed", error=str(exc))
            return False

    def log_event(self, event: AuditEvent) -> None:
        """Append a single audit event as a JSON line."""
        if not self._ensure_dir():
            return

        line = event.to_json() + "\n"
        try:
            with open(self._log_path, "a") as f:
                f.write(line)
        except OSError as exc:
            log.error("audit_write_failed", error=str(exc))
            return

        # Auto-rotate if needed
        try:
            if self._log_path.stat().st_size > MAX_LOG_SIZE_BYTES:
                self.rotate()
        except OSError:
            pass

    def get_recent(self, limit: int = 100) -> list[AuditEvent]:
        """Read the last N events from the log file."""
        if not self._log_path.exists():
            return []

        events: list[AuditEvent] = []
        try:
            with open(self._log_path) as f:
                lines = f.readlines()

            # Take the last `limit` lines
            recent_lines = lines[-limit:] if len(lines) > limit else lines

            for line in recent_lines:
                stripped = line.strip()
                if not stripped:
                    continue
                try:
                    data = json.loads(stripped)
                    events.append(AuditEvent(**data))
                except (json.JSONDecodeError, TypeError, KeyError):
                    continue

        except OSError as exc:
            log.warning("audit_read_failed", error=str(exc))

        return events

    def rotate(self) -> None:
        """Rotate the log file if it exceeds the size limit.

        The current file is moved to .old (overwriting any existing .old).
        A fresh log file is created.
        """
        if not self._log_path.exists():
            return

        try:
            size = self._log_path.stat().st_size
        except OSError:
            return

        if size <= MAX_LOG_SIZE_BYTES:
            return

        old_path = self._log_path.with_suffix(".old")
        try:
            shutil.move(str(self._log_path), str(old_path))
            log.info("audit_log_rotated", old=str(old_path), size=size)
        except OSError as exc:
            log.error("audit_rotate_failed", error=str(exc))

    def event_count(self) -> int:
        """Count the number of events in the log file."""
        if not self._log_path.exists():
            return 0
        try:
            with open(self._log_path) as f:
                return sum(1 for line in f if line.strip())
        except OSError:
            return 0
