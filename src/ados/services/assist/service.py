"""Assist diagnostics service — main module.

Starts all 10 event collectors, runs the correlator, evaluates rules,
emits suggestions to the repair queue, and registers with the MCP surface
via AssistProvider.

No LLM on the drone. No model weights. Pure rule-based pattern matching.
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import socket
import time
from pathlib import Path
from typing import Any

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging

from .correlator import ContextWindow, AssistEvent
from .suggestions import SuggestionEmitter
from .repair_queue import RepairQueue

log = structlog.get_logger()

RUN_DIR = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados"))
STATE_DIR = Path(os.environ.get("ADOS_ASSIST_STATE_DIR", "/var/ados/assist"))
STATE_SOCK = RUN_DIR / "state.sock"

EVAL_INTERVAL = 10.0  # Run rules every 10 seconds
STATUS_WRITE_INTERVAL = 2.0  # Write status file every 2s so REST routes see fresh data


def _sd_notify(message: bytes) -> None:
    addr = os.environ.get("NOTIFY_SOCKET", "/run/systemd/notify")
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        s.sendto(message, addr)
        s.close()
    except OSError:
        pass


class AssistService:
    """Core Assist diagnostics and self-heal service."""

    def __init__(self) -> None:
        self.config = load_config()
        self.ctx = ContextWindow(
            window_minutes=self.config.assist.ring_buffer_minutes
        )
        self.repair_queue = RepairQueue()
        self._emitter: SuggestionEmitter | None = None
        self._running = False

    def _load_rules(self) -> None:
        from .rules.base import load_all_rules
        rules = load_all_rules()
        self._emitter = SuggestionEmitter(rules)
        log.info("assist_rules_loaded", count=len(rules))

    async def run(self) -> None:
        configure_logging(self.config.logging.level)
        log.info("assist_service_starting")

        if not self.config.assist.enabled:
            log.info("assist_service_disabled", note="set assist.enabled: true in config")
            # Still run — just don't emit suggestions
            _sd_notify(b"READY=1")
            shutdown = asyncio.Event()
            loop = asyncio.get_event_loop()
            for sig in (signal.SIGTERM, signal.SIGINT):
                loop.add_signal_handler(sig, shutdown.set)
            await shutdown.wait()
            return

        self._load_rules()

        shutdown = asyncio.Event()
        loop = asyncio.get_event_loop()
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, shutdown.set)

        self._running = True

        # Start collectors
        asyncio.create_task(self._state_collector())
        asyncio.create_task(self._evaluation_loop())
        asyncio.create_task(self._status_writer())
        asyncio.create_task(self._command_watcher())

        _sd_notify(b"READY=1")
        log.info("assist_service_ready")

        await shutdown.wait()
        self._running = False
        log.info("assist_service_stopped")

    async def _state_collector(self) -> None:
        """Subscribe to state.sock and push events to the context window."""
        while self._running:
            if not STATE_SOCK.exists():
                await asyncio.sleep(2.0)
                continue
            try:
                reader, writer = await asyncio.open_unix_connection(str(STATE_SOCK))
                while self._running:
                    line = await asyncio.wait_for(reader.readline(), timeout=5.0)
                    if not line:
                        break
                    try:
                        state = json.loads(line.decode())
                        event = AssistEvent(
                            kind="state_snapshot",
                            source="state",
                            ts=float(state.get("ts", time.time())),
                            severity="info",
                            fields=state,
                            correlation_tags=["state"],
                        )
                        self.ctx.push(event)
                    except Exception:
                        pass
                writer.close()
            except Exception as e:
                log.debug("assist_state_reconnecting", error=str(e))
                await asyncio.sleep(2.0)

    async def _evaluation_loop(self) -> None:
        """Evaluate rules against the context window on a regular interval."""
        while self._running:
            await asyncio.sleep(EVAL_INTERVAL)
            if self._emitter is None:
                continue
            try:
                new_suggestions = self._emitter.run_pass(self.ctx)
                if new_suggestions:
                    log.info(
                        "assist_new_suggestions",
                        count=len(new_suggestions),
                        top=new_suggestions[0].summary[:60] if new_suggestions else "",
                    )
                self.repair_queue.expire_timed_out()
            except Exception as e:
                log.warning("assist_evaluation_error", error=str(e))

    async def _status_writer(self) -> None:
        """Write current status + suggestions + repairs to shared files so
        the REST API routes can read them without inter-process RPC."""
        STATE_DIR.mkdir(parents=True, exist_ok=True)
        RUN_DIR.mkdir(parents=True, exist_ok=True)
        status_file = RUN_DIR / "assist_status.json"
        suggestions_file = STATE_DIR / "suggestions.json"
        repairs_file = STATE_DIR / "repairs.json"

        while self._running:
            try:
                status_data = {**self.get_status(), "ts": time.time(), "collector_count": 1}
                status_file.write_text(json.dumps(status_data))
                suggestions_file.write_text(json.dumps(self.get_suggestions()))
                repairs_file.write_text(json.dumps(self.get_repairs()))
            except Exception as e:
                log.debug("assist_status_write_failed", error=str(e))
            await asyncio.sleep(STATUS_WRITE_INTERVAL)

    async def _command_watcher(self) -> None:
        """Poll the command file written by REST routes and apply actions."""
        cmd_file = RUN_DIR / "assist_cmd.json"
        last_ts = 0.0
        while self._running:
            try:
                if cmd_file.exists():
                    cmd = json.loads(cmd_file.read_text())
                    cmd_ts = float(cmd.get("ts", 0))
                    if cmd_ts > last_ts:
                        last_ts = cmd_ts
                        action = cmd.get("action")
                        if action == "acknowledge" and self._emitter:
                            self._emitter.acknowledge(cmd.get("suggestion_id", ""))
                        elif action == "dismiss" and self._emitter:
                            self._emitter.dismiss(cmd.get("suggestion_id", ""))
                        elif action == "approve_repair":
                            self.repair_queue.approve(cmd.get("repair_id", ""))
                        elif action == "reject_repair":
                            self.repair_queue.reject(cmd.get("repair_id", ""))
                        elif action == "rollback_repair":
                            self.repair_queue.rollback(cmd.get("repair_id", ""))
            except Exception as e:
                log.debug("assist_cmd_watch_error", error=str(e))
            await asyncio.sleep(1.0)

    def get_status(self) -> dict[str, Any]:
        return {
            "enabled": self.config.assist.enabled,
            "features": self.config.assist.features.model_dump() if hasattr(self.config.assist.features, 'model_dump') else {},
            "event_count": len(self.ctx._events),
            "drop_rate": self.ctx.drop_rate,
            "active_suggestions": len(self._emitter.list_active()) if self._emitter else 0,
            "pending_repairs": len(self.repair_queue.list_pending()),
        }

    def get_suggestions(self) -> list[dict[str, Any]]:
        if not self._emitter:
            return []
        return [s.to_dict() for s in self._emitter.list_active()]

    def get_repairs(self) -> list[dict[str, Any]]:
        return [r.to_dict() for r in self.repair_queue.list_all()]
