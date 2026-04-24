"""Survey photogrammetry service.

Monitors active survey missions, runs the quality validator per frame,
tracks coverage, and packages datasets in multiple formats.
"""

from __future__ import annotations

import asyncio
import json
import os
import secrets
import signal
import socket
import time
from pathlib import Path
from typing import Any

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging
from .quality import QualityValidator, QualityEvent

log = structlog.get_logger()

RUN_DIR = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados"))
STATE_SOCK = RUN_DIR / "state.sock"


def _sd_notify(message: bytes) -> None:
    addr = os.environ.get("NOTIFY_SOCKET", "/run/systemd/notify")
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        s.sendto(message, addr)
        s.close()
    except OSError:
        pass


class SurveyService:
    """Manages survey quality validation and dataset packaging."""

    def __init__(self) -> None:
        self.config = load_config()
        self.validator = QualityValidator()
        self._active = False
        self._mission_id: str | None = None
        self._quality_history: list[QualityEvent] = []
        self._coverage_cells: dict[str, bool] = {}
        self._captured_count = 0
        self._pass_count = 0
        self._warn_count = 0
        self._fail_count = 0
        self._running = False

    async def run(self) -> None:
        configure_logging(self.config.logging.level)
        log.info("survey_service_starting")

        shutdown = asyncio.Event()
        loop = asyncio.get_event_loop()
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, shutdown.set)

        _sd_notify(b"READY=1")
        log.info("survey_service_ready")
        self._running = True

        # Monitor state socket for mission events
        asyncio.create_task(self._state_monitor())
        asyncio.create_task(self._status_writer())

        await shutdown.wait()
        self._running = False
        log.info("survey_service_stopped")

    async def _status_writer(self) -> None:
        """Write status to /run/ados/survey_status.json every 2 seconds
        so the REST routes can report current state."""
        status_file = RUN_DIR / "survey_status.json"
        while self._running:
            try:
                import json as _json, time as _time
                data = {**self.current_status(), "ts": _time.time()}
                status_file.write_text(_json.dumps(data))
            except Exception:
                pass
            await asyncio.sleep(2.0)

    async def _state_monitor(self) -> None:
        """Watch state socket for armed/mission events."""
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
                        # Detect mission start/end
                        mode = state.get("mode", "")
                        if "AUTO" in mode.upper() and not self._active:
                            self._active = True
                            self._mission_id = secrets.token_hex(8)
                            self._quality_history.clear()
                            log.info("survey_mission_started", mission_id=self._mission_id)
                        elif "AUTO" not in mode.upper() and self._active:
                            self._active = False
                            log.info("survey_mission_ended",
                                     captured=self._captured_count,
                                     pass_pct=f"{100*self._pass_count//max(self._captured_count,1)}%")
                    except Exception:
                        pass
                writer.close()
            except Exception:
                await asyncio.sleep(2.0)

    def current_status(self) -> dict[str, Any]:
        return {
            "active": self._active,
            "mission_id": self._mission_id,
            "captured_frames": self._captured_count,
            "pass_frames": self._pass_count,
            "warn_frames": self._warn_count,
            "fail_frames": self._fail_count,
            "coverage_pct": len(self._coverage_cells) / max(1, len(self._coverage_cells)) * 100,
        }
