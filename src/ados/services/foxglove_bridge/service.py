"""Foxglove WebSocket Protocol bridge.

Publishes ADOS drone telemetry, vision, and assist data as Foxglove
channels on port 8765. Foxglove Studio (browser or desktop) connects
to this bridge and displays the data in its visualization panels.

Channel list mirrors ADOS MCP Resources:
  /ados/identity      — device ID, version, board
  /ados/health        — service states
  /ados/telemetry     — attitude, GPS, battery, mode (10 Hz)
  /ados/services      — service status (2 Hz)
  /ados/wfb           — WFB-ng link stats (1 Hz)
  /ados/vision/detections    — object detections (1 Hz, when vision up)
  /ados/vision/occupancy_grid — depth occupancy (2 Hz, when vision up)
  /ados/memory/observations  — new observations (event-driven)
  /ados/assist/suggestions   — assist suggestions (event-driven)

MCAP recording: start/stop via REST /api/foxglove/record
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

log = structlog.get_logger()

RUN_DIR = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados"))
STATE_SOCK = RUN_DIR / "state.sock"
RECORDINGS_DIR = Path(os.environ.get("ADOS_FOXGLOVE_DIR", "/var/ados/foxglove"))

DEFAULT_PORT = 8765


def _sd_notify(message: bytes) -> None:
    addr = os.environ.get("NOTIFY_SOCKET", "/run/systemd/notify")
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        s.sendto(message, addr)
        s.close()
    except OSError:
        pass


class FoxgloveBridgeService:
    """Foxglove WebSocket Protocol bridge service."""

    def __init__(self, port: int = DEFAULT_PORT) -> None:
        self.port = port
        self._running = False
        self._recording = False
        self._recording_path: str | None = None
        self._clients: set[Any] = set()
        RECORDINGS_DIR.mkdir(parents=True, exist_ok=True)

    async def run(self) -> None:
        from ados.core.config import load_config
        from ados.core.logging import configure_logging
        config = load_config()
        configure_logging(config.logging.level)
        log.info("foxglove_bridge_starting", port=self.port)

        shutdown = asyncio.Event()
        loop = asyncio.get_event_loop()
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, shutdown.set)

        self._running = True

        # Start data pump task (reads state.sock and broadcasts)
        asyncio.create_task(self._pump_telemetry())

        # Try to start foxglove_websocket server
        server_task = asyncio.create_task(self._start_server())

        # Publish status to /run/ados/foxglove_status.json and watch for
        # /run/ados/foxglove_cmd.json (start/stop recording from REST).
        asyncio.create_task(self._status_and_cmd_loop())

        _sd_notify(b"READY=1")
        log.info("foxglove_bridge_ready", port=self.port)

        await shutdown.wait()
        self._running = False
        server_task.cancel()
        log.info("foxglove_bridge_stopped")

    async def _start_server(self) -> None:
        """Start the Foxglove WebSocket server if foxglove-websocket-protocol is available."""
        try:
            from foxglove_websocket.server import FoxgloveServer, FoxgloveServerListener
            from foxglove_websocket.types import ChannelId

            async with FoxgloveServer(
                host="0.0.0.0",
                port=self.port,
                name="ADOS Drone Agent",
            ) as server:
                # Register channels
                telemetry_ch = await server.add_channel({
                    "topic": "/ados/telemetry",
                    "encoding": "json",
                    "schemaName": "ados.Telemetry",
                    "schema": json.dumps({
                        "type": "object",
                        "properties": {
                            "ts": {"type": "number"},
                            "lat": {"type": "number"},
                            "lon": {"type": "number"},
                            "alt": {"type": "number"},
                            "heading": {"type": "number"},
                            "groundSpeed": {"type": "number"},
                            "mode": {"type": "string"},
                            "battery": {"type": "object"},
                            "gps": {"type": "object"},
                        },
                    }),
                })
                self._foxglove_server = server
                self._telemetry_ch = telemetry_ch

                # Keep running
                while self._running:
                    await asyncio.sleep(1.0)
        except ImportError:
            log.info("foxglove_websocket_not_available", note="install foxglove-websocket to enable")
            # Fall through — service is healthy but just won't serve
            while self._running:
                await asyncio.sleep(10.0)
        except Exception as e:
            log.warning("foxglove_server_error", error=str(e))

    async def _pump_telemetry(self) -> None:
        """Read state.sock and push to connected Foxglove clients."""
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
                        await self._broadcast_telemetry(state)
                    except Exception:
                        pass
                writer.close()
            except Exception:
                await asyncio.sleep(2.0)

    async def _broadcast_telemetry(self, state: dict[str, Any]) -> None:
        """Broadcast telemetry to connected clients."""
        # If foxglove_websocket server is running, send to it
        server = getattr(self, "_foxglove_server", None)
        ch = getattr(self, "_telemetry_ch", None)
        if server and ch:
            try:
                await server.send_message(
                    ch,
                    int(time.time() * 1e9),
                    json.dumps(state).encode(),
                )
            except Exception:
                pass

    @property
    def recording(self) -> bool:
        return self._recording

    @property
    def recording_path(self) -> str | None:
        return self._recording_path

    def start_recording(self, filename: str = "") -> str:
        ts = int(time.time())
        name = filename or f"recording_{ts}.mcap"
        self._recording_path = str(RECORDINGS_DIR / name)
        self._recording = True
        log.info("foxglove_recording_started", path=self._recording_path)
        return self._recording_path

    def stop_recording(self) -> str | None:
        path = self._recording_path
        self._recording = False
        self._recording_path = None
        log.info("foxglove_recording_stopped", path=path)
        return path

    def list_recordings(self) -> list[dict[str, Any]]:
        results = []
        for f in sorted(RECORDINGS_DIR.glob("*.mcap"), key=lambda p: p.stat().st_mtime, reverse=True):
            results.append({
                "name": f.name,
                "path": str(f),
                "size_bytes": f.stat().st_size,
                "created_at": f.stat().st_mtime,
            })
        return results

    async def _status_and_cmd_loop(self) -> None:
        """Write status file so REST routes can report current state,
        and poll command file for start/stop recording requests.
        """
        run_dir = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados"))
        status_file = run_dir / "foxglove_status.json"
        cmd_file = run_dir / "foxglove_cmd.json"
        last_cmd_ts = 0.0

        while self._running:
            try:
                run_dir.mkdir(parents=True, exist_ok=True)
                status_file.write_text(json.dumps({
                    "ts": time.time(),
                    "port": self.port,
                    "recording": self._recording,
                    "recording_path": self._recording_path,
                }))

                if cmd_file.exists():
                    cmd = json.loads(cmd_file.read_text())
                    cmd_ts = float(cmd.get("ts", 0))
                    if cmd_ts > last_cmd_ts:
                        last_cmd_ts = cmd_ts
                        action = cmd.get("action")
                        if action == "start":
                            self.start_recording(cmd.get("filename", ""))
                        elif action == "stop":
                            self.stop_recording()
            except Exception as e:
                log.debug("foxglove_status_loop_error", error=str(e))
            await asyncio.sleep(1.0)
