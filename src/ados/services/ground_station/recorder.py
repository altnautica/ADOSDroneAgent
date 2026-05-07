"""Ground-station video recorder.

Records the live drone video stream as it arrives at the ground node,
served back to clients as MP4 files. The recorder taps the local
mediamtx RTSP source (`rtsp://127.0.0.1:8554/main`) rather than
reaching for the upstream UDP payload directly. mediamtx is the
canonical local source on the ground side: ffmpeg already does the
UDP-to-RTSP muxing in `mediamtx_manager.py`, so this recorder only
needs to consume RTSP and remux to MP4 with `-c copy` (no transcode).

Lifecycle mirrors the air-side recorder pattern:

* `start()` spawns `ffmpeg -i rtsp://127.0.0.1:8554/main -c copy <path>.mp4`.
* `stop()` sends SIGTERM, waits up to 5s, escalates to SIGKILL on timeout.
* `list_recordings()` enumerates `.mp4` files with size + mtime.
* `is_active()` reports whether a capture is in flight.

If mediamtx stops publishing (RTSP disconnect) ffmpeg exits cleanly with
EOF and `is_active()` reflects that. A returncode-watch task picks up
mid-stream crashes so the next `/status` poll surfaces the truth.
"""

from __future__ import annotations

import asyncio
import os
import shutil
import signal
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import RECORDINGS_DIR

log = get_logger("ground_station.recorder")

# Local RTSP source published by mediamtx_manager. Kept in sync with
# `GROUND_RTSP_PATH` and the rtsp_port constant there.
_DEFAULT_RTSP_URL = "rtsp://127.0.0.1:8554/main"

# Stop sequence timeouts.
_SIGTERM_GRACE_SECONDS = 5.0


@dataclass
class RecordingFile:
    """Metadata for a single recording file on disk."""

    filename: str
    size_bytes: int
    mtime: float

    def to_dict(self) -> dict:
        return {
            "filename": self.filename,
            "size_bytes": self.size_bytes,
            "mtime": self.mtime,
        }


class RecorderError(Exception):
    """Raised for recoverable recorder failures (disk full, ffmpeg missing)."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


class GroundStationRecorder:
    """Captures the local mediamtx RTSP stream into MP4 files on disk.

    Singleton-friendly. Use `get_recorder()` rather than constructing
    directly so the rest of the agent can read recorder state without
    threading a handle through every call site.
    """

    def __init__(
        self,
        recording_dir: Path | str = RECORDINGS_DIR,
        rtsp_url: str = _DEFAULT_RTSP_URL,
    ) -> None:
        self._dir = Path(recording_dir)
        self._rtsp_url = rtsp_url
        self._process: asyncio.subprocess.Process | None = None
        self._current_path: Path | None = None
        self._started_at: float = 0.0
        self._watcher: asyncio.Task | None = None
        self._lock = asyncio.Lock()

    # ------------------------------------------------------------------
    # Public state
    # ------------------------------------------------------------------

    def is_active(self) -> bool:
        """True if a recording subprocess is currently running."""
        proc = self._process
        if proc is None:
            return False
        return proc.returncode is None

    @property
    def current_filename(self) -> str | None:
        if self._current_path is None:
            return None
        return self._current_path.name

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self, filename_hint: str | None = None) -> dict:
        """Spawn ffmpeg to record the local RTSP stream.

        Returns a dict with `filename`, `started_at` (ISO 8601 UTC), and
        `path`. Raises `RecorderError` with a code when start cannot
        proceed.
        """
        async with self._lock:
            if self.is_active():
                raise RecorderError(
                    "E_RECORDING_ACTIVE",
                    "a recording is already in progress",
                )

            ffmpeg_bin = shutil.which("ffmpeg")
            if not ffmpeg_bin:
                raise RecorderError(
                    "E_FFMPEG_NOT_FOUND",
                    "ffmpeg binary not on PATH",
                )

            try:
                self._dir.mkdir(parents=True, exist_ok=True)
            except OSError as exc:
                raise RecorderError(
                    "E_RECORDING_DIR_UNWRITABLE",
                    f"cannot create recordings directory: {exc}",
                ) from exc

            try:
                free_bytes = shutil.disk_usage(str(self._dir)).free
            except OSError:
                free_bytes = None
            if free_bytes is not None and free_bytes < 64 * 1024 * 1024:
                raise RecorderError(
                    "E_DISK_FULL",
                    "less than 64 MiB free on the recordings volume",
                )

            filename = self._generate_filename(filename_hint)
            output_path = self._dir / filename

            cmd = [
                ffmpeg_bin,
                "-y",
                "-rtsp_transport", "tcp",
                "-i", self._rtsp_url,
                "-c", "copy",
                "-movflags", "+faststart",
                str(output_path),
            ]

            try:
                self._process = await asyncio.create_subprocess_exec(
                    *cmd,
                    stdin=asyncio.subprocess.DEVNULL,
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.PIPE,
                )
            except (OSError, FileNotFoundError) as exc:
                self._process = None
                raise RecorderError(
                    "E_RECORDER_SPAWN_FAILED",
                    f"failed to spawn ffmpeg: {exc}",
                ) from exc

            self._current_path = output_path
            self._started_at = time.monotonic()
            self._watcher = asyncio.create_task(self._watch_subprocess())

            started_iso = datetime.now(timezone.utc).isoformat()
            log.info(
                "recording_started",
                filename=filename,
                path=str(output_path),
                rtsp_url=self._rtsp_url,
            )
            return {
                "filename": filename,
                "started_at": started_iso,
                "path": str(output_path),
            }

    async def stop(self) -> dict:
        """Gracefully stop the in-flight recording.

        Sends SIGTERM, waits up to 5s, escalates to SIGKILL on timeout.
        Returns a dict with `filename`, `stopped_at` (ISO 8601 UTC),
        `duration_seconds`, and `size_bytes`. Raises `RecorderError`
        when no recording is active.
        """
        async with self._lock:
            proc = self._process
            path = self._current_path
            if proc is None or path is None or proc.returncode is not None:
                raise RecorderError(
                    "E_RECORDING_NOT_ACTIVE",
                    "no recording is currently active",
                )

            try:
                proc.send_signal(signal.SIGTERM)
            except ProcessLookupError:
                pass

            try:
                await asyncio.wait_for(
                    proc.wait(), timeout=_SIGTERM_GRACE_SECONDS
                )
            except TimeoutError:
                log.warning(
                    "recorder_sigterm_timeout",
                    filename=path.name,
                    grace_s=_SIGTERM_GRACE_SECONDS,
                )
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
                try:
                    await asyncio.wait_for(proc.wait(), timeout=2.0)
                except TimeoutError:
                    pass

            duration = max(0.0, time.monotonic() - self._started_at)
            stopped_iso = datetime.now(timezone.utc).isoformat()

            size_bytes = 0
            try:
                size_bytes = path.stat().st_size
            except OSError:
                pass

            self._process = None
            self._current_path = None
            self._started_at = 0.0
            if self._watcher is not None and not self._watcher.done():
                self._watcher.cancel()
            self._watcher = None

            log.info(
                "recording_stopped",
                filename=path.name,
                duration_s=round(duration, 2),
                size_bytes=size_bytes,
            )
            return {
                "filename": path.name,
                "stopped_at": stopped_iso,
                "duration_seconds": round(duration, 2),
                "size_bytes": size_bytes,
            }

    def list_recordings(self) -> list[RecordingFile]:
        """Enumerate `.mp4` files in the recordings dir, newest first."""
        if not self._dir.is_dir():
            return []
        items: list[RecordingFile] = []
        for entry in self._dir.iterdir():
            if not entry.is_file() or entry.suffix.lower() != ".mp4":
                continue
            try:
                stat = entry.stat()
            except OSError:
                continue
            items.append(
                RecordingFile(
                    filename=entry.name,
                    size_bytes=stat.st_size,
                    mtime=stat.st_mtime,
                )
            )
        items.sort(key=lambda r: r.mtime, reverse=True)
        return items

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _generate_filename(self, hint: str | None) -> str:
        """Timestamped filename. Hint is sanitised and appended."""
        ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H-%M-%S")
        if not hint:
            return f"{ts}.mp4"
        safe = "".join(c for c in hint if c.isalnum() or c in ("-", "_"))[:48]
        if not safe:
            return f"{ts}.mp4"
        return f"{ts}_{safe}.mp4"

    async def _watch_subprocess(self) -> None:
        """Wait on the subprocess and clear state when it exits.

        Covers the cases where ffmpeg exits on its own (RTSP EOF when
        mediamtx stops publishing, source disconnect, disk full mid-
        write). Stop() handles the operator-initiated path.
        """
        proc = self._process
        if proc is None:
            return
        try:
            rc = await proc.wait()
        except asyncio.CancelledError:
            return
        # Only clear state if stop() did not already.
        if self._process is proc:
            duration = max(0.0, time.monotonic() - self._started_at)
            path = self._current_path
            log.info(
                "recording_subprocess_exited",
                returncode=rc,
                filename=path.name if path is not None else None,
                duration_s=round(duration, 2),
            )
            self._process = None
            self._current_path = None
            self._started_at = 0.0
            self._watcher = None


# ----------------------------------------------------------------------
# Module-level singleton
# ----------------------------------------------------------------------
# Same pattern as get_pair_manager(), get_input_manager(), get_pic_arbiter().
_instance: GroundStationRecorder | None = None


def get_recorder() -> GroundStationRecorder:
    """Return the process-wide GroundStationRecorder singleton."""
    global _instance
    if _instance is None:
        _instance = GroundStationRecorder()
    return _instance


def _reset_for_tests() -> None:
    """Drop the cached singleton. Test-only helper."""
    global _instance
    if _instance is not None and _instance.is_active():
        # Best-effort kill so a leaked subprocess does not survive teardown.
        try:
            proc = _instance._process
            if proc is not None and proc.returncode is None:
                os.kill(proc.pid, signal.SIGKILL)
        except (ProcessLookupError, AttributeError):
            pass
    _instance = None
