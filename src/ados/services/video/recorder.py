"""Video recorder — MP4 capture with storage management."""

from __future__ import annotations

import asyncio
import os
import shutil
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("video.recorder")

_DISK_USAGE_THRESHOLD = 80.0  # percent


@dataclass
class RecordingInfo:
    """Metadata for a recorded video file."""

    filename: str
    path: str
    size_bytes: int
    timestamp: str
    duration_seconds: float = 0.0

    def to_dict(self) -> dict:
        return {
            "filename": self.filename,
            "path": self.path,
            "size_bytes": self.size_bytes,
            "timestamp": self.timestamp,
            "duration_seconds": self.duration_seconds,
        }


class VideoRecorder:
    """Records video streams to MP4 files with automatic storage management.

    Uses ffmpeg to mux the incoming stream into an MP4 container.  When disk
    usage exceeds 80%, the oldest recordings are deleted automatically.
    """

    def __init__(self, recording_dir: str = "/var/ados/recordings") -> None:
        self._dir = Path(recording_dir)
        self._process: asyncio.subprocess.Process | None = None
        self._current_path: str = ""
        self._recording = False
        self._start_time: float = 0.0

    @property
    def recording(self) -> bool:
        return self._recording

    @property
    def current_path(self) -> str:
        return self._current_path

    def _ensure_dir(self) -> None:
        """Create the recording directory if it does not exist."""
        self._dir.mkdir(parents=True, exist_ok=True)

    def _generate_filename(self) -> str:
        """Generate a timestamped filename for a new recording."""
        ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
        return f"recording_{ts}.mp4"

    async def start_recording(self, source: str = "-") -> str:
        """Start recording from a source into an MP4 file.

        Args:
            source: Input source for ffmpeg (pipe, device, or URL).

        Returns:
            The file path of the new recording.
        """
        if self._recording:
            log.warning("recording_already_active", path=self._current_path)
            return self._current_path

        self._ensure_dir()
        self._cleanup_old_recordings()

        filename = self._generate_filename()
        filepath = str(self._dir / filename)

        cmd = [
            "ffmpeg",
            "-y",
            "-i", source,
            "-c", "copy",
            "-movflags", "+faststart",
            filepath,
        ]

        try:
            self._process = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            self._current_path = filepath
            self._recording = True
            self._start_time = time.monotonic()
            log.info("recording_started", path=filepath)
        except FileNotFoundError:
            log.error("ffmpeg_not_found", msg="ffmpeg required for recording")
            return ""

        return filepath

    async def stop_recording(self) -> str:
        """Stop the current recording and return the file path."""
        if not self._recording or self._process is None:
            log.warning("no_active_recording")
            return ""

        filepath = self._current_path

        # Send 'q' to ffmpeg stdin to trigger graceful shutdown
        if self._process.stdin:
            try:
                self._process.stdin.write(b"q")
                await self._process.stdin.drain()
            except (BrokenPipeError, ConnectionResetError):
                pass

        try:
            await asyncio.wait_for(self._process.wait(), timeout=10.0)
        except TimeoutError:
            self._process.kill()
            await self._process.wait()

        duration = time.monotonic() - self._start_time
        self._recording = False
        self._process = None
        self._current_path = ""

        log.info("recording_stopped", path=filepath, duration_s=round(duration, 1))
        return filepath

    def get_recordings(self) -> list[RecordingInfo]:
        """List all recordings in the recording directory."""
        if not self._dir.is_dir():
            return []

        recordings: list[RecordingInfo] = []
        for entry in sorted(self._dir.iterdir()):
            if not entry.is_file() or not entry.suffix == ".mp4":
                continue
            stat = entry.stat()
            ts = datetime.fromtimestamp(stat.st_mtime, tz=timezone.utc).isoformat()
            recordings.append(RecordingInfo(
                filename=entry.name,
                path=str(entry),
                size_bytes=stat.st_size,
                timestamp=ts,
            ))
        return recordings

    def _cleanup_old_recordings(self) -> None:
        """Delete oldest recordings if disk usage exceeds the threshold."""
        try:
            usage = shutil.disk_usage(str(self._dir))
        except OSError:
            return

        used_pct = (usage.used / usage.total) * 100.0
        if used_pct <= _DISK_USAGE_THRESHOLD:
            return

        recordings = self.get_recordings()
        if not recordings:
            return

        # Sort by timestamp ascending (oldest first)
        recordings.sort(key=lambda r: r.timestamp)

        for rec in recordings:
            if used_pct <= _DISK_USAGE_THRESHOLD:
                break
            try:
                os.remove(rec.path)
                log.info("recording_deleted", path=rec.path, reason="disk_cleanup")
                # Recheck usage
                usage = shutil.disk_usage(str(self._dir))
                used_pct = (usage.used / usage.total) * 100.0
            except OSError as exc:
                log.warning("recording_delete_failed", path=rec.path, error=str(exc))

    def to_dict(self) -> dict:
        """Serialize recorder state for API responses."""
        return {
            "recording": self._recording,
            "current_path": self._current_path,
            "recordings_dir": str(self._dir),
        }
