"""MCP audit log.

Append-only JSONL at /var/ados/mcp/audit/YYYY-MM-DD.jsonl.

Every MCP operation is recorded with:
  ts         ISO-8601 timestamp
  token_id   abbreviated (first 8 chars)
  client_hint caller label
  event      tool_call | resource_read | subscribe | unsubscribe | gate_block | pair | revoke
  target     tool name or resource URI
  outcome    SUCCESS | ERROR | GATE_BLOCKED
  latency_ms round-trip milliseconds
  args_sha256 SHA-256 of JSON args for write operations (null for reads)

Read events are sampled at 1/N (default 1/100) to prevent log flooding.
All write, gate-block, and lifecycle events are always recorded.
"""

from __future__ import annotations

import hashlib
import json
import os
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Literal

import structlog

log = structlog.get_logger()

AuditOutcome = Literal["SUCCESS", "ERROR", "GATE_BLOCKED"]
AuditEvent = Literal[
    "tool_call", "resource_read", "subscribe", "unsubscribe",
    "gate_block", "pair", "revoke"
]


class AuditLog:
    """Rotating JSONL audit log for MCP operations."""

    ALWAYS_LOG_EVENTS: frozenset[AuditEvent] = frozenset({
        "tool_call", "gate_block", "pair", "revoke",
    })

    def __init__(
        self,
        log_dir: str,
        rotate_mb: int = 50,
        read_sample_rate: int = 100,
    ) -> None:
        self._dir = Path(log_dir)
        self._rotate_bytes = rotate_mb * 1024 * 1024
        self._sample_rate = read_sample_rate
        self._counter = 0
        self._current_file: Path | None = None
        self._current_handle = None

    def _ensure_dir(self) -> None:
        self._dir.mkdir(parents=True, exist_ok=True)

    def _today_path(self) -> Path:
        today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
        return self._dir / f"{today}.jsonl"

    def _open(self) -> None:
        self._ensure_dir()
        path = self._today_path()
        self._current_file = path
        self._current_handle = open(path, "a", encoding="utf-8")

    def _should_rotate(self) -> bool:
        if self._current_file is None:
            return True
        today_path = self._today_path()
        if today_path != self._current_file:
            return True
        try:
            size = self._current_file.stat().st_size
            return size >= self._rotate_bytes
        except OSError:
            return True

    def _should_record(self, event: AuditEvent, outcome: AuditOutcome) -> bool:
        """Decide whether to write this entry.
        Writes, gate blocks, and lifecycle events always write.
        Reads are sampled at 1/N.
        """
        if event in self.ALWAYS_LOG_EVENTS:
            return True
        if outcome == "GATE_BLOCKED":
            return True
        # Sampled read/subscribe events.
        self._counter += 1
        return (self._counter % self._sample_rate) == 0

    def record(
        self,
        token_id: str,
        client_hint: str,
        event: AuditEvent,
        target: str,
        outcome: AuditOutcome,
        latency_ms: float,
        args_sha256: str | None = None,
    ) -> None:
        if not self._should_record(event, outcome):
            return

        entry = {
            "ts": datetime.now(timezone.utc).isoformat(),
            "token_id": token_id[:8] if token_id else "anon",
            "client_hint": client_hint,
            "event": event,
            "target": target,
            "outcome": outcome,
            "latency_ms": round(latency_ms, 2),
            "args_sha256": args_sha256,
        }

        try:
            if self._should_rotate():
                if self._current_handle:
                    self._current_handle.close()
                self._open()

            line = json.dumps(entry) + "\n"
            self._current_handle.write(line)  # type: ignore[union-attr]
            self._current_handle.flush()  # type: ignore[union-attr]
        except OSError as e:
            log.warning("mcp_audit_write_failed", error=str(e))

    def tail(self, n: int = 100) -> list[dict]:
        """Return the last N audit entries across all log files."""
        self._ensure_dir()
        entries: list[dict] = []
        for f in sorted(self._dir.glob("*.jsonl"), reverse=True):
            if len(entries) >= n:
                break
            try:
                lines = f.read_text().splitlines()
                for line in reversed(lines):
                    if len(entries) >= n:
                        break
                    try:
                        entries.append(json.loads(line))
                    except json.JSONDecodeError:
                        pass
            except OSError:
                pass
        return list(reversed(entries))

    def close(self) -> None:
        if self._current_handle:
            try:
                self._current_handle.close()
            except OSError:
                pass
            self._current_handle = None


def args_sha256(args: dict) -> str:
    """Return SHA-256 hex of the JSON-serialized args dict."""
    return hashlib.sha256(json.dumps(args, sort_keys=True).encode()).hexdigest()
