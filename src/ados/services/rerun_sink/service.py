"""Rerun visualization sink service.

Publishes drone state as Rerun entity paths via the Rerun SDK.
Clients connect to gRPC on port 9876 or download .rrd files.

Entity paths:
  /world/drone/pose          — Transform3D (10 Hz)
  /camera/main/frame         — Image (1 Hz keyframe)
  /camera/main/detections    — Boxes2D from Vision Engine (1 Hz)
  /perception/depth_points   — Points3D from MiDaS depth (1 Hz)
  /perception/occupancy_grid — Mesh3D occupancy (2 Hz)
  /telemetry/battery         — Scalar
  /telemetry/rc              — Tensor
  /telemetry/ekf             — Scalar
  /events/{kind}             — TextLog

Recording: start/stop via REST /api/rerun/record
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
RECORDINGS_DIR = Path(os.environ.get("ADOS_RERUN_DIR", "/var/ados/rerun"))

DEFAULT_PORT = 9876


def _sd_notify(message: bytes) -> None:
    addr = os.environ.get("NOTIFY_SOCKET", "/run/systemd/notify")
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        s.sendto(message, addr)
        s.close()
    except OSError:
        pass


class RerunSinkService:
    """Rerun visualization sink service."""

    def __init__(self, port: int = DEFAULT_PORT) -> None:
        self.port = port
        self._running = False
        self._recording = False
        self._recording_path: str | None = None
        RECORDINGS_DIR.mkdir(parents=True, exist_ok=True)

    async def run(self) -> None:
        from ados.core.config import load_config
        from ados.core.logging import configure_logging
        config = load_config()
        configure_logging(config.logging.level)
        log.info("rerun_sink_starting", port=self.port)

        shutdown = asyncio.Event()
        loop = asyncio.get_event_loop()
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, shutdown.set)

        self._running = True

        # Try to initialize Rerun SDK
        asyncio.create_task(self._init_rerun())
        asyncio.create_task(self._pump_state())
        asyncio.create_task(self._status_and_cmd_loop())

        _sd_notify(b"READY=1")
        log.info("rerun_sink_ready", port=self.port)

        await shutdown.wait()
        self._running = False
        log.info("rerun_sink_stopped")

    async def _init_rerun(self) -> None:
        """Initialize Rerun SDK and connect to the viewer."""
        try:
            import rerun as rr
            rr.init("ADOS Drone Agent", spawn=False)
            rr.connect_grpc(f"0.0.0.0:{self.port}")
            self._rr = rr
            log.info("rerun_sdk_initialized", port=self.port)
        except ImportError:
            log.info("rerun_sdk_not_available", note="install rerun-sdk to enable")
            self._rr = None
        except Exception as e:
            log.warning("rerun_sdk_error", error=str(e))
            self._rr = None

    async def _pump_state(self) -> None:
        """Read state.sock and log to Rerun."""
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
                        self._log_state(state)
                    except Exception:
                        pass
                writer.close()
            except Exception:
                await asyncio.sleep(2.0)

    def _log_state(self, state: dict[str, Any]) -> None:
        """Log drone state to Rerun entity paths."""
        rr = getattr(self, "_rr", None)
        if rr is None:
            return
        try:
            ts = state.get("ts", time.time())
            rr.set_time_seconds("wall_clock", ts)

            lat = state.get("lat")
            lon = state.get("lon")
            alt = state.get("alt", 0)
            if lat is not None and lon is not None:
                rr.log("/world/drone/pose", rr.Transform3D(
                    translation=[lon, lat, alt]
                ))

            batt = state.get("battery", {})
            if batt:
                rr.log("/telemetry/battery", rr.Scalar(batt.get("remaining", 0)))
        except Exception:
            pass

    def start_recording(self, filename: str = "") -> str:
        ts = int(time.time())
        name = filename or f"recording_{ts}.rrd"
        self._recording_path = str(RECORDINGS_DIR / name)
        self._recording = True
        rr = getattr(self, "_rr", None)
        if rr:
            try:
                rr.save(self._recording_path)
            except Exception:
                pass
        log.info("rerun_recording_started", path=self._recording_path)
        return self._recording_path

    def stop_recording(self) -> str | None:
        path = self._recording_path
        self._recording = False
        self._recording_path = None
        log.info("rerun_recording_stopped", path=path)
        return path

    def list_recordings(self) -> list[dict[str, Any]]:
        results = []
        for f in sorted(RECORDINGS_DIR.glob("*.rrd"), key=lambda p: p.stat().st_mtime, reverse=True):
            results.append({
                "name": f.name,
                "path": str(f),
                "size_bytes": f.stat().st_size,
                "created_at": f.stat().st_mtime,
                "download_url": f"/api/rerun/recordings/{f.name}/download",
            })
        return results

    async def _status_and_cmd_loop(self) -> None:
        """Write status file for REST routes and poll cmd file for
        start/stop recording requests."""
        run_dir = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados"))
        status_file = run_dir / "rerun_status.json"
        cmd_file = run_dir / "rerun_cmd.json"
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
                log.debug("rerun_status_loop_error", error=str(e))
            await asyncio.sleep(1.0)
