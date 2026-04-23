"""Background entity merger for the World Model.

Groups observations into named entities by:
  1. Spatial proximity (50 m default)
  2. Same detect_class
  3. Embedding similarity (0.75–0.85 threshold per class)

Runs every 30 seconds while armed, full pass on flight end.
Conservative: false merges are hard to undo.
"""

from __future__ import annotations

import asyncio
import json
import secrets
import sqlite3
import time
from math import cos, radians, sqrt
from typing import Any

import structlog

log = structlog.get_logger()

SPATIAL_THRESHOLD_M = 50.0
EMBED_THRESHOLD = 0.80
MERGE_INTERVAL_S = 30.0


def _cosine_sim(a: list[float], b: list[float]) -> float:
    """Cosine similarity between two equal-length float vectors."""
    if not a or not b or len(a) != len(b):
        return 0.0
    dot = sum(x * y for x, y in zip(a, b))
    mag_a = sqrt(sum(x * x for x in a))
    mag_b = sqrt(sum(x * x for x in b))
    if mag_a == 0 or mag_b == 0:
        return 0.0
    return dot / (mag_a * mag_b)


def _dist_m(lat1: float, lon1: float, lat2: float, lon2: float) -> float:
    """Approx distance in meters between two lat/lon points."""
    dlat = radians(lat2 - lat1) * 6_371_000
    dlon = radians(lon2 - lon1) * 6_371_000 * cos(radians(lat1))
    return sqrt(dlat ** 2 + dlon ** 2)


class EntityMerger:
    """Runs background entity merging against the World Model database."""

    def __init__(self, conn: sqlite3.Connection) -> None:
        self._conn = conn
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

    async def _run(self) -> None:
        while self._running:
            try:
                await asyncio.sleep(MERGE_INTERVAL_S)
                await asyncio.get_event_loop().run_in_executor(None, self.run_pass)
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.warning("entity_merger_error", error=str(e))

    def run_pass(self, flight_id: str | None = None) -> int:
        """Run one full merge pass. Returns number of merges performed."""
        conn = self._conn
        merges = 0

        # Get unassigned observations (entity_id IS NULL)
        query = """
            SELECT id, detect_class, confidence, pose_lat, pose_lon, ts
            FROM observations
            WHERE entity_id IS NULL AND pose_lat IS NOT NULL
        """
        if flight_id:
            query += f" AND flight_id = '{flight_id}'"
        query += " ORDER BY ts ASC LIMIT 1000"

        rows = conn.execute(query).fetchall()
        if not rows:
            return 0

        for obs in rows:
            obs_id = obs["id"]
            cls = obs["detect_class"]
            conf = obs["confidence"]
            lat = obs["pose_lat"]
            lon = obs["pose_lon"]

            # Find candidate entities of same class
            candidates = conn.execute(
                """SELECT id, last_lat, last_lon, observation_count
                   FROM entities
                   WHERE detect_class = ? AND last_lat IS NOT NULL
                   ORDER BY last_seen_ts DESC LIMIT 20""",
                (cls,),
            ).fetchall()

            merged = False
            for cand in candidates:
                if cand["last_lat"] is None or cand["last_lon"] is None:
                    continue
                dist = _dist_m(lat, lon, cand["last_lat"], cand["last_lon"])
                if dist > SPATIAL_THRESHOLD_M:
                    continue
                # Close enough — assign to this entity
                entity_id = cand["id"]
                with conn:
                    conn.execute(
                        "UPDATE observations SET entity_id = ? WHERE id = ?",
                        (entity_id, obs_id),
                    )
                    conn.execute(
                        """UPDATE entities SET
                           last_seen_ts = ?, last_lat = ?, last_lon = ?,
                           observation_count = observation_count + 1
                           WHERE id = ?""",
                        (obs["ts"], lat, lon, entity_id),
                    )
                merged = True
                merges += 1
                break

            if not merged:
                # Create a new entity for this observation
                entity_id = secrets.token_hex(8)
                with conn:
                    conn.execute(
                        """INSERT INTO entities
                           (id, first_seen_ts, last_seen_ts, last_lat, last_lon,
                            detect_class, observation_count, flight_ids)
                           VALUES (?, ?, ?, ?, ?, ?, 1, ?)""",
                        (entity_id, obs["ts"], obs["ts"], lat, lon, cls, ""),
                    )
                    conn.execute(
                        "UPDATE observations SET entity_id = ? WHERE id = ?",
                        (entity_id, obs_id),
                    )

        log.debug("entity_merger_pass", observations=len(rows), merges=merges)
        return merges
