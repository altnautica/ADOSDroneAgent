"""5-stage ingest pipeline for the World Model.

Stages:
  1. PoseFuser      — interpolate drone pose to detection timestamp
  2. Projector      — pixel bbox → world lat/lon/alt
  3. Embedder       — CLIP 768-dim (calls Vision Engine gRPC if available)
  4. Captioner      — VLM caption (calls Vision Engine gRPC on tier-3+)
  5. RulesFilter    — YAML capture-rules engine (first-match-wins)

Inputs via Unix socket /run/ados/detections.sock (Vision Engine)
and /run/ados/state.sock (MAVLink service, 10 Hz pose stream).

Each incoming detection event triggers the full pipeline.
Outputs are staged into the WriteBatcher for batched SQLite writes.
"""

from __future__ import annotations

import asyncio
import json
import os
import secrets
import socket
import struct
import time
from collections import deque
from pathlib import Path
from typing import Any

import structlog

from .capture_rules import RulesEngine
from .writer import FrameRow, ObservationRow, WriteBatcher

log = structlog.get_logger()

RUN_DIR = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados"))
DETECTIONS_SOCK = RUN_DIR / "detections.sock"
STATE_SOCK = RUN_DIR / "state.sock"
POSE_RING_SIZE = 20       # Keep last 2s of 10 Hz pose stream
POSE_STALE_S = 0.5        # Reject detections with pose older than 500ms
FRAME_RATE_HZ = 1.0       # Keyframe capture rate (1 per second default)


class PoseRing:
    """Ring buffer of recent drone pose snapshots."""

    def __init__(self, max_size: int = POSE_RING_SIZE) -> None:
        self._ring: deque[dict[str, Any]] = deque(maxlen=max_size)

    def push(self, state: dict[str, Any]) -> None:
        self._ring.append({
            "ts": state.get("ts", time.time()),
            "lat": state.get("lat"),
            "lon": state.get("lon"),
            "alt": state.get("alt"),
            "heading": state.get("heading"),
        })

    def interpolate(self, ts: float) -> dict[str, Any] | None:
        """Return the pose closest to ts, or None if too stale."""
        if not self._ring:
            return None
        closest = min(self._ring, key=lambda p: abs(p["ts"] - ts))
        if abs(closest["ts"] - ts) > POSE_STALE_S:
            return None
        return closest


class Projector:
    """Projects pixel bbox to world lat/lon using pinhole model + AGL."""

    def project(
        self,
        bbox_px: dict[str, float] | None,
        pose: dict[str, Any],
        camera_fov_deg: float = 90.0,
        frame_width_px: int = 1920,
        frame_height_px: int = 1080,
    ) -> dict[str, Any] | None:
        """Return world bbox dict or None if projection not possible."""
        lat = pose.get("lat")
        lon = pose.get("lon")
        alt = pose.get("alt", 0)
        if lat is None or lon is None:
            return None
        if bbox_px is None:
            return {"lat": lat, "lon": lon, "alt": alt}

        # Centre of bbox in normalized image coords [-0.5, 0.5]
        cx = (bbox_px.get("x", 0) + bbox_px.get("w", 0) / 2) / frame_width_px - 0.5
        cy = (bbox_px.get("y", 0) + bbox_px.get("h", 0) / 2) / frame_height_px - 0.5

        # Simple flat-Earth projection: use altitude as depth
        from math import tan, radians, cos
        half_fov = radians(camera_fov_deg / 2)
        scale = alt * tan(half_fov)

        # dx/dy in meters (approximation)
        dx_m = cx * scale * 2
        dy_m = cy * scale * 2

        # Convert meters to degrees
        R = 6_371_000
        from math import radians as rad, degrees as deg, cos as _cos
        d_lat = deg(dy_m / R)
        d_lon = deg(dx_m / (R * _cos(rad(lat)))) if lat != 0 else 0

        return {
            "lat": lat + d_lat,
            "lon": lon + d_lon,
            "alt": alt,
            "width_m": round(bbox_px.get("w", 0) / frame_width_px * scale * 2, 2),
            "height_m": round(bbox_px.get("h", 0) / frame_height_px * scale * 2, 2),
        }


class IngestPipeline:
    """5-stage ingest pipeline for World Model observation events."""

    def __init__(
        self,
        batcher: WriteBatcher,
        rules_engine: RulesEngine,
        flight_id: str,
        thumb_dir: Path,
        fullres_dir: Path,
    ) -> None:
        self._batcher = batcher
        self._rules = rules_engine
        self._flight_id = flight_id
        self._pose_ring = PoseRing()
        self._projector = Projector()
        self._last_frame_ts: float = 0
        self._running = False
        self._tasks: list[asyncio.Task] = []
        self._thumb_dir = thumb_dir
        self._fullres_dir = fullres_dir

    def set_flight_id(self, flight_id: str) -> None:
        self._flight_id = flight_id

    async def start(self) -> None:
        self._running = True
        self._tasks.append(asyncio.create_task(self._pose_reader()))
        self._tasks.append(asyncio.create_task(self._detection_reader()))

    async def stop(self) -> None:
        self._running = False
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()

    # ── Stage 0: pose reader ───────────────────────────────────────────────

    async def _pose_reader(self) -> None:
        """Subscribe to /run/ados/state.sock and push poses to ring."""
        while self._running:
            if not STATE_SOCK.exists():
                await asyncio.sleep(1.0)
                continue
            try:
                reader, writer = await asyncio.open_unix_connection(str(STATE_SOCK))
                log.info("ingest_pose_connected")
                while self._running:
                    line = await asyncio.wait_for(reader.readline(), timeout=5.0)
                    if not line:
                        break
                    try:
                        state = json.loads(line.decode())
                        state["ts"] = state.get("ts") or time.time()
                        self._pose_ring.push(state)
                    except json.JSONDecodeError:
                        pass
                writer.close()
            except Exception as e:
                log.debug("ingest_pose_reconnecting", error=str(e))
                await asyncio.sleep(2.0)

    # ── Stage 0: detection reader ──────────────────────────────────────────

    async def _detection_reader(self) -> None:
        """Subscribe to /run/ados/detections.sock and run the full pipeline."""
        while self._running:
            if not DETECTIONS_SOCK.exists():
                await asyncio.sleep(2.0)
                continue
            try:
                reader, writer = await asyncio.open_unix_connection(str(DETECTIONS_SOCK))
                log.info("ingest_detections_connected")
                while self._running:
                    raw = await asyncio.wait_for(reader.readline(), timeout=5.0)
                    if not raw:
                        break
                    try:
                        event = json.loads(raw.decode())
                        await self._process(event)
                    except json.JSONDecodeError:
                        pass
                writer.close()
            except Exception as e:
                log.debug("ingest_detections_reconnecting", error=str(e))
                await asyncio.sleep(2.0)

    # ── Stages 1–5: full pipeline ──────────────────────────────────────────

    async def _process(self, event: dict[str, Any]) -> None:
        det_ts = float(event.get("ts", time.time()))
        detect_class = str(event.get("class", "unknown"))
        confidence = float(event.get("confidence", 0.0))
        bbox_px = event.get("bbox_px")
        model_name = str(event.get("model", "unknown"))

        # Stage 1: Pose fuse
        pose = self._pose_ring.interpolate(det_ts)
        if pose is None:
            return  # Pose too stale — discard

        # Stage 2: Projection
        world_bbox = self._projector.project(bbox_px, pose)

        # Stage 5: Capture rules (decide before expensive embedding)
        lat = world_bbox["lat"] if world_bbox else pose.get("lat")
        lon = world_bbox["lon"] if world_bbox else pose.get("lon")
        rule_result = self._rules.evaluate(detect_class, confidence, lat, lon)
        if not rule_result.persist:
            return

        # Keyframe capture (1 Hz default)
        frame_id = None
        now = time.monotonic()
        if (now - self._last_frame_ts) >= (1.0 / FRAME_RATE_HZ):
            frame_id = secrets.token_hex(8)
            self._last_frame_ts = now
            frame_row = FrameRow(
                id=frame_id,
                flight_id=self._flight_id,
                ts=det_ts,
                pose_lat=pose.get("lat"),
                pose_lon=pose.get("lon"),
                pose_alt_m=pose.get("alt"),
                pose_heading=pose.get("heading"),
                camera_id="main",
                thumb_path=None,
                fullres_path=None,
                caption=None,
                caption_source=None,
                caption_model=None,
            )
            await self._batcher.put_frame(frame_row)

        # Stage 3: Embedding (async, best-effort)
        embedding: list[float] | None = None
        try:
            embedding = await asyncio.wait_for(
                self._get_embedding(event.get("crop_base64")),
                timeout=2.0,
            )
        except Exception:
            pass

        # Stage 4: Caption (async, best-effort, tier-3+ only)
        # (VLM captioning handled separately in the VLM service)

        # Write observation
        obs_id = secrets.token_hex(10)
        obs_row = ObservationRow(
            id=obs_id,
            flight_id=self._flight_id,
            frame_id=frame_id,
            ts=det_ts,
            detect_class=detect_class,
            confidence=confidence,
            pose_lat=lat,
            pose_lon=lon,
            pose_alt_m=pose.get("alt"),
            target_lat=world_bbox.get("lat") if world_bbox else None,
            target_lon=world_bbox.get("lon") if world_bbox else None,
            target_alt_m=world_bbox.get("alt") if world_bbox else None,
            bbox_px=json.dumps(bbox_px) if bbox_px else None,
            bbox_world=json.dumps(world_bbox) if world_bbox else None,
            source="vision",
            model=model_name,
            tags=rule_result.tags,
            embedding=embedding,
        )
        await self._batcher.put_observation(obs_row)

    async def _get_embedding(self, crop_base64: str | None) -> list[float] | None:
        """Call Vision Engine gRPC for a CLIP embedding. Best-effort."""
        if crop_base64 is None:
            return None
        # Vision Engine gRPC endpoint is at localhost:50051 (when running).
        # This is a best-effort call — if the service is not running, return None.
        try:
            import httpx
            resp = await httpx.AsyncClient(timeout=1.0).post(
                "http://127.0.0.1:8080/api/vision/embed",
                json={"crop_base64": crop_base64},
            )
            data = resp.json()
            return data.get("embedding")
        except Exception:
            return None
