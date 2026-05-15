"""Headless SEI sample reader for the drone-side video pipeline.

Reads the local mediamtx RTSP feed (which carries SEI-stamped frames
after `wrap_with_sei_inject` moved the injector upstream of mediamtx)
through a thin ffmpeg subprocess, walks the H.264 Annex-B byte stream
for the ADOS latency SEI marker, and persists rolling EWMA stats to
``/run/ados/lcd-latency.json`` at 1 Hz.

This decouples the `/api/video/latency` endpoint from the LCD-bound
`LocalVideoTap` inside the OLED service. On a drone profile with no
local LCD attached (e.g. groundnode after the 2026-05-08 swap), the
OLED tap never runs and the latency file never gets written, so the
GCS popover AIR row stayed blank even with SEI on. This tap fills the
gap by being unconditional whenever wfb.sei_latency is true.

The tap is best-effort: any subprocess failure, parse error, or
ffmpeg restart loop is logged and the latency file simply stops
updating — the GCS popover degrades to "measuring..." via the
existing graceful-degrade path.
"""

from __future__ import annotations

import asyncio
import json
import os
import time
from pathlib import Path
from typing import Any

from ados.core.logging import get_logger
from ados.services.video.local_tap import parse_sei_latency_ns

log = get_logger("video.sei_tap")

# EWMA smoothing factor — same as LocalVideoTap to keep the JSON
# file's `ewma_ms` field semantically consistent across both writers.
_EWMA_ALPHA = 0.25
# Read chunk size for the ffmpeg stdout pipe. 64 KiB keeps the SEI
# scan loop tight without inflating per-iteration overhead.
_READ_CHUNK = 64 * 1024
# How often we flush the rolling stats to disk. Mirrors the LCD-side
# cadence so consumers see the same update rate regardless of source.
_PERSIST_INTERVAL_S = 1.0
# Default output file. Keeps the existing `/api/video/latency`
# endpoint working without any agent-side route changes.
_LCD_LATENCY_PATH = Path("/run/ados/lcd-latency.json")
# Restart cool-down to avoid hot-looping a broken ffmpeg.
_RESTART_BACKOFF_S = 2.0


class HeadlessSeiTap:
    """Spawns ffmpeg + reads the RTSP stream for SEI markers.

    Not tied to any GStreamer pipeline or LCD display. The constructor
    only stores configuration; call `start()` from an async context.
    """

    def __init__(
        self,
        rtsp_url: str,
        *,
        output_path: Path = _LCD_LATENCY_PATH,
    ) -> None:
        self._rtsp_url = rtsp_url
        self._output_path = output_path
        self._proc: asyncio.subprocess.Process | None = None
        self._reader_task: asyncio.Task | None = None
        self._stderr_task: asyncio.Task | None = None
        self._stop = False
        # Rolling latency state.
        self._latency_ewma_ms: float | None = None
        self._last_latency_ms: float | None = None
        self._sample_count_window = 0
        self._sample_count_last_flush = 0
        self._last_flush_at = time.monotonic()

    async def start(self) -> None:
        """Begin reading. Idempotent; no-op if already running."""
        if self._reader_task is not None and not self._reader_task.done():
            return
        self._stop = False
        self._reader_task = asyncio.create_task(self._run_loop())
        log.info("headless_sei_tap_started", rtsp=self._rtsp_url, output=str(self._output_path))

    async def stop(self) -> None:
        """Tear down the subprocess + reader. Idempotent."""
        self._stop = True
        proc = self._proc
        self._proc = None
        if proc is not None and proc.returncode is None:
            try:
                proc.terminate()
            except ProcessLookupError:
                pass
            try:
                await asyncio.wait_for(proc.wait(), timeout=3.0)
            except (TimeoutError, asyncio.CancelledError):
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
        if self._reader_task is not None and not self._reader_task.done():
            self._reader_task.cancel()
            try:
                await self._reader_task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
        if self._stderr_task is not None and not self._stderr_task.done():
            self._stderr_task.cancel()
            try:
                await self._stderr_task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
        self._reader_task = None
        self._stderr_task = None
        log.info("headless_sei_tap_stopped")

    async def _run_loop(self) -> None:
        """Outer loop. Restarts ffmpeg with backoff on any failure."""
        while not self._stop:
            try:
                await self._read_once()
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001
                log.warning("headless_sei_tap_read_failed", error=str(exc))
            if self._stop:
                break
            await asyncio.sleep(_RESTART_BACKOFF_S)

    async def _read_once(self) -> None:
        """One ffmpeg session. Reads SEI samples until ffmpeg exits."""
        # `-c copy -f h264 pipe:1` emits raw Annex-B H.264 to stdout
        # without re-encoding. `-rtsp_transport tcp` mirrors the rest
        # of the pipeline's RTSP TCP discipline.
        self._proc = await asyncio.create_subprocess_exec(
            "ffmpeg",
            "-loglevel", "error",
            "-fflags", "nobuffer",
            "-flags", "low_delay",
            "-rtsp_transport", "tcp",
            "-i", self._rtsp_url,
            "-c:v", "copy",
            "-f", "h264",
            "pipe:1",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        proc = self._proc
        assert proc.stdout is not None
        self._stderr_task = asyncio.create_task(self._drain_stderr(proc))

        buf = bytearray()
        try:
            while not self._stop:
                chunk = await proc.stdout.read(_READ_CHUNK)
                if not chunk:
                    break
                buf.extend(chunk)
                # Trim left-over leading bytes once the buffer is
                # comfortably larger than a single access unit so we
                # don't grow unbounded between SEI matches.
                if len(buf) > 4 * _READ_CHUNK:
                    # Keep the tail; the next SEI marker is more likely
                    # to land near the freshest bytes.
                    del buf[: len(buf) - 2 * _READ_CHUNK]
                ns = parse_sei_latency_ns(bytes(buf))
                if ns is None:
                    continue
                # Compute latency against our wall clock. SEI carries
                # `time.time_ns()` at encode time; both sides rely on
                # chrony/systemd-timesyncd to stay within a few ms.
                delta_ns = time.time_ns() - ns
                if delta_ns <= 0 or delta_ns > 60_000_000_000:
                    # Sample is bogus (clock skew, stale buffer); skip.
                    continue
                delta_ms = delta_ns / 1_000_000
                self._last_latency_ms = delta_ms
                self._sample_count_window += 1
                if self._latency_ewma_ms is None:
                    self._latency_ewma_ms = delta_ms
                else:
                    self._latency_ewma_ms = (
                        _EWMA_ALPHA * delta_ms
                        + (1.0 - _EWMA_ALPHA) * self._latency_ewma_ms
                    )
                # Reset the buffer to just past the parsed SEI so the
                # next iteration doesn't re-find the same marker.
                buf.clear()

                now = time.monotonic()
                if now - self._last_flush_at >= _PERSIST_INTERVAL_S:
                    await self._persist()
                    self._last_flush_at = now
        finally:
            if proc.returncode is None:
                try:
                    proc.terminate()
                except ProcessLookupError:
                    pass
            try:
                await asyncio.wait_for(proc.wait(), timeout=3.0)
            except (TimeoutError, asyncio.CancelledError):
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass

    async def _drain_stderr(self, proc: asyncio.subprocess.Process) -> None:
        """Forward ffmpeg's stderr to the agent log at WARNING level."""
        if proc.stderr is None:
            return
        try:
            while True:
                line = await proc.stderr.readline()
                if not line:
                    return
                text = line.decode("utf-8", errors="replace").rstrip()
                if not text:
                    continue
                log.warning("headless_sei_tap_stderr", line=text)
        except asyncio.CancelledError:
            raise
        except Exception:  # noqa: BLE001
            return

    async def _persist(self) -> None:
        """Write the rolling stats to /run/ados/lcd-latency.json."""
        samples_since_last = self._sample_count_window - self._sample_count_last_flush
        self._sample_count_last_flush = self._sample_count_window
        payload: dict[str, Any] = {
            "latency_ms": (
                round(self._last_latency_ms, 2)
                if self._last_latency_ms is not None
                else None
            ),
            "latency_ewma_ms": (
                round(self._latency_ewma_ms, 2)
                if self._latency_ewma_ms is not None
                else None
            ),
            "pipeline_latency_ms": None,
            "samples": samples_since_last,
            "source": "sei",
        }
        try:
            self._output_path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self._output_path.with_suffix(self._output_path.suffix + ".tmp")
            tmp.write_text(json.dumps(payload))
            os.replace(tmp, self._output_path)
        except OSError as exc:
            log.warning("headless_sei_tap_persist_failed", error=str(exc))
