# SPDX-License-Identifier: GPL-3.0-only
# Copyright (c) 2026 Altnautica
"""MCAP recording manager for the ADOS Drone Agent ROS 2 environment.

Records ROS 2 topic data to MCAP files via ``ros2 bag record`` running
inside the Docker container. Provides start, stop, list, and metadata
queries. Enforces a rotation policy to prevent disk exhaustion.

Recording directory: /var/ados/ros/recordings/ on the host,
bind-mounted into the container.
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import structlog

log = structlog.get_logger("ados.services.ros_recording")

CONTAINER_NAME = "ados-ros"
RECORDINGS_DIR = Path("/var/ados/ros/recordings")
MAX_RECORDINGS = 10
MAX_TOTAL_SIZE_MB = 5120  # 5 GB


@dataclass
class ActiveRecording:
    """Tracks a running bag recording process."""

    recording_id: str
    pid: int
    topics: list[str]
    output_dir: str
    started_at: float = field(default_factory=time.time)


class RecordingManager:
    """Manages MCAP bag recordings inside the ROS container."""

    def __init__(self) -> None:
        self._active: dict[str, ActiveRecording] = {}
        RECORDINGS_DIR.mkdir(parents=True, exist_ok=True)

    @property
    def active_recordings(self) -> dict[str, ActiveRecording]:
        return dict(self._active)

    async def start(
        self,
        topics: list[str] | None = None,
        output_dir: str | None = None,
        max_size_mb: int = 500,
        max_duration_s: int = 0,
    ) -> str:
        """Start recording topics to an MCAP file.

        Args:
            topics: List of topic names to record. If None, records all topics.
            output_dir: Override output directory inside the container.
            max_size_mb: Maximum file size in MB (0 = no limit).
            max_duration_s: Maximum duration in seconds (0 = no limit).

        Returns:
            A recording_id string for later reference.

        Raises:
            RuntimeError: If the recording process fails to start.
        """
        recording_id = uuid.uuid4().hex[:12]
        container_out = output_dir or f"/var/ados/ros/recordings/{recording_id}"

        # Ensure the host directory exists
        host_out = RECORDINGS_DIR / recording_id
        host_out.mkdir(parents=True, exist_ok=True)

        cmd_parts = [
            "source /opt/ros/jazzy/setup.bash &&",
            "source /opt/ados/ros-packages/install/setup.bash &&",
            "ros2 bag record -s mcap",
            f"-o {container_out}/bag",
        ]

        if max_size_mb > 0:
            cmd_parts.append(f"--max-bag-size {max_size_mb * 1024 * 1024}")

        if max_duration_s > 0:
            cmd_parts.append(f"--max-bag-duration {max_duration_s}")

        if topics:
            cmd_parts.append("--topics")
            cmd_parts.extend(topics)
        else:
            cmd_parts.append("--all")

        full_cmd = " ".join(cmd_parts)

        proc = await asyncio.create_subprocess_exec(
            "docker", "exec", "-d", CONTAINER_NAME,
            "bash", "-c", full_cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        await proc.communicate()

        # Find the PID of the recording process inside the container
        pid = await self._find_recording_pid()
        if pid is None:
            log.warning("recording started but PID not found", recording_id=recording_id)
            pid = 0

        rec = ActiveRecording(
            recording_id=recording_id,
            pid=pid,
            topics=topics or ["--all"],
            output_dir=container_out,
        )
        self._active[recording_id] = rec

        # Apply rotation policy
        await self._enforce_rotation()

        log.info("recording started", recording_id=recording_id, topics=rec.topics)
        return recording_id

    async def stop(self, recording_id: str) -> bool:
        """Stop a running recording by its ID.

        Args:
            recording_id: The ID returned from start().

        Returns:
            True if stopped, False if not found.
        """
        rec = self._active.pop(recording_id, None)
        if rec is None:
            log.warning("recording not found", recording_id=recording_id)
            return False

        if rec.pid > 0:
            try:
                subprocess.run(
                    ["docker", "exec", CONTAINER_NAME, "kill", str(rec.pid)],
                    capture_output=True,
                    timeout=10,
                )
            except (subprocess.TimeoutExpired, FileNotFoundError):
                pass

        log.info("recording stopped", recording_id=recording_id)
        return True

    async def stop_all(self) -> int:
        """Stop all active recordings. Returns the count stopped."""
        ids = list(self._active.keys())
        count = 0
        for rid in ids:
            if await self.stop(rid):
                count += 1
        return count

    def list_recordings(self) -> list[dict[str, Any]]:
        """List MCAP recording directories with metadata.

        Returns:
            List of dicts with name, path, size_mb, file_count, and created fields.
        """
        recordings: list[dict[str, Any]] = []

        if not RECORDINGS_DIR.is_dir():
            return recordings

        for entry in sorted(RECORDINGS_DIR.iterdir(), reverse=True):
            if not entry.is_dir():
                continue

            # Calculate total size
            total_bytes = 0
            file_count = 0
            for f in entry.rglob("*"):
                if f.is_file():
                    total_bytes += f.stat().st_size
                    file_count += 1

            # Get creation time from directory mtime
            import datetime
            mtime = entry.stat().st_mtime
            created = datetime.datetime.fromtimestamp(
                mtime, tz=datetime.timezone.utc
            ).isoformat()

            is_active = entry.name in self._active

            recordings.append({
                "name": entry.name,
                "path": str(entry),
                "size_mb": round(total_bytes / (1024 * 1024), 2),
                "file_count": file_count,
                "created": created,
                "active": is_active,
            })

        return recordings

    def get_metadata(self, recording_id: str) -> dict[str, Any] | None:
        """Read metadata from an MCAP recording.

        Looks for the bag metadata YAML file that ros2 bag record writes.

        Args:
            recording_id: Recording directory name.

        Returns:
            Dict with topic_count, message_count, duration_s, start_time, end_time,
            and topics list. None if not found.
        """
        rec_dir = RECORDINGS_DIR / recording_id
        if not rec_dir.is_dir():
            return None

        # Look for metadata.yaml written by ros2 bag
        meta_files = list(rec_dir.rglob("metadata.yaml"))
        if not meta_files:
            # Fallback: just return file info
            mcap_files = list(rec_dir.rglob("*.mcap"))
            return {
                "recording_id": recording_id,
                "mcap_files": [str(f) for f in mcap_files],
                "total_size_mb": sum(f.stat().st_size for f in mcap_files) / (1024 * 1024),
                "topic_count": 0,
                "message_count": 0,
                "duration_s": 0.0,
                "topics": [],
            }

        try:
            import yaml
            with open(meta_files[0]) as f:
                meta = yaml.safe_load(f)

            if not meta or "rosbag2_bagfile_information" not in meta:
                return None

            bag_info = meta["rosbag2_bagfile_information"]
            topics_with_counts = bag_info.get("topics_with_message_count", [])
            topic_list = []
            total_messages = 0
            for t in topics_with_counts:
                topic_meta = t.get("topic_metadata", {})
                count = t.get("message_count", 0)
                total_messages += count
                topic_list.append({
                    "name": topic_meta.get("name", ""),
                    "type": topic_meta.get("type", ""),
                    "message_count": count,
                })

            duration_ns = bag_info.get("duration", {}).get("nanoseconds", 0)

            return {
                "recording_id": recording_id,
                "topic_count": len(topic_list),
                "message_count": total_messages,
                "duration_s": round(duration_ns / 1e9, 2),
                "starting_time": bag_info.get("starting_time", {}).get("nanoseconds_since_epoch", 0),
                "topics": topic_list,
            }

        except Exception as exc:
            log.warning("failed to parse recording metadata", error=str(exc))
            return None

    async def _find_recording_pid(self) -> int | None:
        """Find the PID of a ros2 bag record process in the container."""
        try:
            result = subprocess.run(
                [
                    "docker", "exec", CONTAINER_NAME,
                    "pgrep", "-f", "ros2 bag record",
                ],
                capture_output=True,
                timeout=5,
            )
            if result.returncode == 0:
                pid_str = result.stdout.decode().strip().splitlines()
                if pid_str:
                    return int(pid_str[0])
        except (subprocess.TimeoutExpired, ValueError, FileNotFoundError):
            pass
        return None

    async def _enforce_rotation(self) -> None:
        """Delete old recordings if limits are exceeded."""
        if not RECORDINGS_DIR.is_dir():
            return

        dirs = sorted(
            [d for d in RECORDINGS_DIR.iterdir() if d.is_dir()],
            key=lambda d: d.stat().st_mtime,
        )

        # Skip active recordings
        active_names = set(self._active.keys())
        removable = [d for d in dirs if d.name not in active_names]

        # Remove oldest first if count exceeds limit
        while len(removable) > MAX_RECORDINGS:
            oldest = removable.pop(0)
            log.info("removing old recording (count limit)", path=str(oldest))
            import shutil
            shutil.rmtree(oldest, ignore_errors=True)

        # Remove oldest first if total size exceeds limit
        total_mb = 0.0
        for d in removable:
            for f in d.rglob("*"):
                if f.is_file():
                    total_mb += f.stat().st_size / (1024 * 1024)

        while total_mb > MAX_TOTAL_SIZE_MB and removable:
            oldest = removable.pop(0)
            dir_size = sum(
                f.stat().st_size for f in oldest.rglob("*") if f.is_file()
            ) / (1024 * 1024)
            log.info("removing old recording (size limit)", path=str(oldest), size_mb=dir_size)
            import shutil
            shutil.rmtree(oldest, ignore_errors=True)
            total_mb -= dir_size


# Singleton
_manager: RecordingManager | None = None


def get_recording_manager() -> RecordingManager:
    """Get or create the singleton recording manager."""
    global _manager
    if _manager is None:
        _manager = RecordingManager()
    return _manager
