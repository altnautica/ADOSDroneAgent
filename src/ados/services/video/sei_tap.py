"""Headless SEI sample reader for the drone-side video pipeline.

Reads the local mediamtx RTSP feed (which carries SEI-stamped frames
after `wrap_with_sei_inject` moved the injector upstream of mediamtx)
through a thin ffmpeg subprocess, walks the H.264 Annex-B byte stream
for the ADOS latency SEI marker, and persists rolling EWMA stats to
``/run/ados/lcd-latency.json`` at 1 Hz.

This decouples the `/api/video/latency` endpoint from the LCD-bound
LCD-side tap that used to live inside the OLED service. On a drone profile with no
local LCD attached, the
OLED tap never runs and the latency file never gets written, so the
GCS popover AIR row stayed blank even with SEI on. This tap fills the
gap by being unconditional whenever wfb.sei_latency is true.

On a node following the simulator's clock (``--clock-feed``, the agent's
sim-clock relay socket) the frames are a simulated camera's, each carrying
the World Engine frame identity SEI with its ``sim_time_ns``: the tap then
reports a frame's age on ADOS time (sim now minus the frame's sim time), the
only latency that means anything when sim time runs at N x wall time.
``clock`` in the file says which (``sim`` or ``wall``), and ``sim_time_ns``
carries the newest frame's sim time.

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
from ados.sdk.clock import SimFeed
from ados.services.video.sei_parser import parse_sei_latency_ns, parse_we_frame_id

log = get_logger("video.sei_tap")

# EWMA smoothing factor — unchanged from the LCD-side tap so the JSON
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
        sim_clock: SimFeed | None = None,
    ) -> None:
        self._rtsp_url = rtsp_url
        self._output_path = output_path
        # ADOS time on a node following the simulator's clock; None on wall.
        self._sim_clock = sim_clock
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
        # The newest simulated frame's sim time, ns.
        self._last_sim_time_ns: int | None = None

    @property
    def clock(self) -> str:
        """``sim`` when latency is a frame's age on ADOS time, else ``wall``."""
        return "wall" if self._sim_clock is None else "sim"

    def take_sample(self, data: bytes) -> bool:
        """Read the newest marker in ``data`` into the rolling stats.

        On the sim clock the latency is a simulated frame's age on ADOS time
        (its SEI ``sim_time_ns`` against the simulator's time now); on wall
        time it is the latency SEI's stamp against this host's clock. Returns
        True when ``data`` held a marker (the caller drops the bytes read)."""
        frame = parse_we_frame_id(data)
        stamp = parse_sei_latency_ns(data)
        if frame is None and stamp is None:
            return False
        if frame is not None:
            self._last_sim_time_ns = frame.sim_time_ns
        if self._sim_clock is not None:
            now = self._sim_clock.sim_ns()
            if frame is None or now is None or self._sim_clock.stale():
                return True
            delta_ns = now - frame.sim_time_ns
            # Within one grant ADOS time may not have reached the frame yet.
            if delta_ns < 0 or delta_ns > 60_000_000_000:
                return True
        else:
            if stamp is None:
                return True
            # The SEI carries `time.time_ns()` stamped by the injector, which
            # `encoder.wrap_with_sei_inject` splices AFTER the encoder, so
            # this is stamp-to-readback: muxing, publish and drone-side
            # buffering, not capture or encode. Both sides rely on
            # chrony/systemd-timesyncd to stay within a few ms.
            delta_ns = time.time_ns() - stamp
            if delta_ns <= 0 or delta_ns > 60_000_000_000:
                # Sample is bogus (clock skew, stale buffer); skip.
                return True
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
        return True

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
                if not self.take_sample(bytes(buf)):
                    continue
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

    def payload(self) -> dict[str, Any]:
        """The rolling stats as written to the latency file."""
        samples_since_last = self._sample_count_window - self._sample_count_last_flush
        self._sample_count_last_flush = self._sample_count_window
        return {
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
            "samples": samples_since_last,
            "source": "sei",
            "clock": self.clock,
            "sim_time_ns": self._last_sim_time_ns,
        }

    async def _persist(self) -> None:
        """Write the rolling stats to /run/ados/lcd-latency.json."""
        payload = self.payload()
        try:
            self._output_path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self._output_path.with_suffix(self._output_path.suffix + ".tmp")
            tmp.write_text(json.dumps(payload))
            os.replace(tmp, self._output_path)
        except OSError as exc:
            log.warning("headless_sei_tap_persist_failed", error=str(exc))


def _run_once_main(argv: list[str] | None = None) -> int:
    """`python -m ados.services.video.sei_tap --once --rtsp <url>`.

    One ffmpeg session of the SEI reader, then exit. The video
    orchestrator spawns this as a supervised subprocess (gated on the
    mediamtx path being ready) so latency telemetry is written to
    ``/run/ados/lcd-latency.json`` on a drone with no LCD attached. The
    long-running ``_run_loop`` / ``start`` path is untouched so the
    in-process OLED tap caller is unaffected.
    """
    import argparse

    parser = argparse.ArgumentParser(
        prog="ados.services.video.sei_tap",
        description="One-shot SEI latency sample reader.",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="Run a single ffmpeg read session then exit.",
    )
    parser.add_argument(
        "--rtsp",
        required=True,
        help="Local mediamtx RTSP URL to read SEI markers from.",
    )
    parser.add_argument(
        "--clock-feed",
        default=None,
        help="The agent's sim-clock relay socket: measure a simulated frame's age on ADOS time.",
    )
    args = parser.parse_args(argv)

    sim_clock = None
    if args.clock_feed:
        sim_clock = SimFeed()
        sim_clock.follow(args.clock_feed)
    tap = HeadlessSeiTap(rtsp_url=args.rtsp, sim_clock=sim_clock)
    try:
        asyncio.run(tap._read_once())
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(_run_once_main())
