"""Write batcher for the World Model.

Batches up to 256 rows or 5 seconds (whichever comes first),
then executes a single SQLite transaction per batch.
Handles graceful flush on shutdown.
"""

from __future__ import annotations

import asyncio
import sqlite3
import time
from dataclasses import dataclass, field
from typing import Any

import structlog

log = structlog.get_logger()

BATCH_MAX_ROWS = 256
BATCH_MAX_SECONDS = 5.0
QUEUE_MAX = 1024


@dataclass
class FrameRow:
    id: str
    flight_id: str
    ts: float
    pose_lat: float | None
    pose_lon: float | None
    pose_alt_m: float | None
    pose_heading: float | None
    camera_id: str
    thumb_path: str | None
    fullres_path: str | None
    caption: str | None
    caption_source: str | None
    caption_model: str | None
    captured_by: str = "ingest"
    embedding: list[float] | None = field(default=None, compare=False)
    rownum: int | None = None


@dataclass
class ObservationRow:
    id: str
    flight_id: str
    frame_id: str | None
    ts: float
    detect_class: str
    confidence: float
    pose_lat: float | None
    pose_lon: float | None
    pose_alt_m: float | None
    target_lat: float | None
    target_lon: float | None
    target_alt_m: float | None
    bbox_px: str | None
    bbox_world: str | None
    source: str = "vision"
    model: str = "unknown"
    tags: list[str] = field(default_factory=list)
    embedding: list[float] | None = field(default=None, compare=False)
    rownum: int | None = None


class WriteBatcher:
    """Async write batcher for World Model rows."""

    def __init__(self, conn: sqlite3.Connection) -> None:
        self._conn = conn
        self._frames: list[FrameRow] = []
        self._observations: list[ObservationRow] = []
        self._last_flush = time.monotonic()
        self._queue: asyncio.Queue[Any] = asyncio.Queue(maxsize=QUEUE_MAX)
        self._running = False
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        self._running = True
        self._task = asyncio.create_task(self._run())

    async def stop(self) -> None:
        self._running = False
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
        # Final flush
        self._flush_now()

    async def put_frame(self, row: FrameRow) -> None:
        try:
            self._queue.put_nowait(("frame", row))
        except asyncio.QueueFull:
            log.warning("world_model_queue_full", dropped="frame")

    async def put_observation(self, row: ObservationRow) -> None:
        try:
            self._queue.put_nowait(("obs", row))
        except asyncio.QueueFull:
            log.warning("world_model_queue_full", dropped="observation")

    async def _run(self) -> None:
        while self._running:
            try:
                elapsed = time.monotonic() - self._last_flush
                timeout = max(0.1, BATCH_MAX_SECONDS - elapsed)
                try:
                    kind, row = await asyncio.wait_for(self._queue.get(), timeout=timeout)
                    if kind == "frame":
                        self._frames.append(row)
                    else:
                        self._observations.append(row)
                except asyncio.TimeoutError:
                    pass

                total = len(self._frames) + len(self._observations)
                elapsed = time.monotonic() - self._last_flush
                if total >= BATCH_MAX_ROWS or elapsed >= BATCH_MAX_SECONDS:
                    self._flush_now()
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.warning("write_batcher_error", error=str(e))

    def _flush_now(self) -> None:
        if not self._frames and not self._observations:
            self._last_flush = time.monotonic()
            return
        try:
            with self._conn:
                self._write_frames(self._frames)
                self._write_observations(self._observations)
            log.debug(
                "world_model_batch_written",
                frames=len(self._frames),
                observations=len(self._observations),
            )
        except Exception as e:
            log.warning("world_model_batch_failed", error=str(e))
        finally:
            self._frames.clear()
            self._observations.clear()
            self._last_flush = time.monotonic()

    def _write_frames(self, rows: list[FrameRow]) -> None:
        if not rows:
            return
        self._conn.executemany(
            """INSERT OR IGNORE INTO frames
               (id, flight_id, ts, pose_lat, pose_lon, pose_alt_m, pose_heading,
                camera_id, thumb_path, fullres_path, caption, caption_source, caption_model, captured_by)
               VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
            [
                (
                    r.id, r.flight_id, r.ts, r.pose_lat, r.pose_lon, r.pose_alt_m,
                    r.pose_heading, r.camera_id, r.thumb_path, r.fullres_path,
                    r.caption, r.caption_source, r.caption_model, r.captured_by,
                )
                for r in rows
            ],
        )

    def _write_observations(self, rows: list[ObservationRow]) -> None:
        if not rows:
            return
        self._conn.executemany(
            """INSERT OR IGNORE INTO observations
               (id, flight_id, frame_id, ts, detect_class, confidence,
                pose_lat, pose_lon, pose_alt_m, target_lat, target_lon, target_alt_m,
                bbox_px, bbox_world, source, model)
               VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
            [
                (
                    r.id, r.flight_id, r.frame_id, r.ts, r.detect_class, r.confidence,
                    r.pose_lat, r.pose_lon, r.pose_alt_m,
                    r.target_lat, r.target_lon, r.target_alt_m,
                    r.bbox_px, r.bbox_world, r.source, r.model,
                )
                for r in rows
            ],
        )
        # Write tags
        for r in rows:
            for tag in r.tags:
                self._conn.execute(
                    """INSERT OR IGNORE INTO observation_tags (observation_id, tag, source, applied_at)
                       VALUES (?, ?, 'rule', ?)""",
                    (r.id, tag, r.ts),
                )
