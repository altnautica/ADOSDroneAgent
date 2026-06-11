"""Latency + air-pipeline status routes.

* ``GET /video/latency`` — most-recent SEI-probe glass-to-glass latency
  written by the LCD-side local tap.
* ``GET /v1/video/air-pipeline`` — live stats snapshot from the
  air-side GStreamer pipeline (written by ``ados-video`` at 1 Hz).

Both read the durable logging store first (the store's sidecar tailer
samples the same ``lcd-latency.json`` / ``air-pipeline.json`` files into a
time-aligned series + a state event) and fall back to the live file read
when the store is unreachable or the producer has not been running. The
sidecar files keep being written exactly as before, so the live path is the
unchanged fallback and the response shape is identical on either path.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException
from fastapi.responses import Response

from ados.api.sources.video import latest_air_pipeline, latest_video_latency
from ados.core.logging import get_logger

log = get_logger("api.video")

router = APIRouter()


def _read_latency_live() -> dict[str, Any]:
    """The live SEI-latency read: the unchanged file-backed fallback.

    Reads the state file written by the LCD-side local tap when the SEI
    latency feature is enabled (WfbConfig.sei_latency = true). Returns
    latency_ms=None when the probe is disabled or no SEI samples have arrived
    yet.
    """
    try:
        from ados.core.paths import LCD_LATENCY_STATS_PATH

        path = LCD_LATENCY_STATS_PATH
    except (ImportError, AttributeError):
        path = Path("/run/ados/lcd-latency.json")

    if not Path(str(path)).is_file():
        return {"latency_ms": None, "source": "unavailable"}
    try:
        import json

        blob = json.loads(Path(str(path)).read_text())
    except (OSError, ValueError) as exc:
        log.warning("video_latency_read_failed", error=str(exc))
        return {"latency_ms": None, "source": "read_failed"}
    if not isinstance(blob, dict):
        return {"latency_ms": None, "source": "unexpected_shape"}
    return {
        "latency_ms": blob.get("latency_ms"),
        "ewma_ms": blob.get("latency_ewma_ms") or blob.get("ewma_ms"),
        "pipeline_latency_ms": blob.get("pipeline_latency_ms"),
        "samples": blob.get("samples"),
        "source": blob.get("source", "sei"),
    }


@router.get("/video/latency")
async def get_video_latency() -> dict[str, Any]:
    """Return the most recent SEI-probe glass-to-glass latency.

    Reads the store first; falls back to the live ``lcd-latency.json`` read
    when the store is unreachable or the SEI probe has produced no samples, so
    the route degrades to the same ``{latency_ms: None, source: ...}`` shape it
    always did.
    """
    derived = await latest_video_latency()
    if derived is not None:
        return derived
    return _read_latency_live()


def _read_air_pipeline_live_blob():
    """The live air-pipeline read: the raw blob, or a Response/Exception.

    Returns the parsed dict on success, a 204 ``Response`` when the file is
    absent or not a dict (the legacy bash air pipeline owns the stream), and
    raises ``HTTPException(503)`` on a read/parse error. This is the unchanged
    file-backed fallback the store-first path falls through to.
    """
    from ados.core.paths import AIR_PIPELINE_STATS_PATH

    if not AIR_PIPELINE_STATS_PATH.exists():
        return Response(status_code=204)
    try:
        import json

        blob = json.loads(AIR_PIPELINE_STATS_PATH.read_text())
    except (OSError, ValueError) as exc:
        log.warning("air_pipeline_status_read_failed", error=str(exc))
        raise HTTPException(
            status_code=503, detail="air pipeline stats unavailable"
        ) from exc
    if not isinstance(blob, dict):
        return Response(status_code=204)
    return blob


@router.get("/v1/video/air-pipeline")
async def get_air_pipeline_status():
    """Return the air-side GStreamer pipeline's live stats snapshot.

    Reads the store first; the three monotonic-clock floats the store cannot
    carry (``started_at`` / ``last_state_change_at`` / ``last_buffer_at``) are
    filled from the live ``air-pipeline.json`` blob when it is present. Falls
    back wholesale to the live file read when the store is unreachable or the
    air pipeline is not running, preserving the 204 (not in use) and 503
    (read error) contract.
    """
    derived = await latest_air_pipeline()
    if derived is not None:
        # The store carries every field but the three monotonic floats; merge
        # those from the live file when it is present so the snapshot is whole.
        # A raised live-float read (a read/parse error that the live-only path
        # turns into a 503) must not sink the otherwise-fresh store snapshot:
        # the three floats stay None (already set by the store path), which is
        # strictly better than a 503 when every other field is present.
        try:
            live = _read_air_pipeline_live_blob()
        except (HTTPException, OSError, ValueError):
            live = None
        if isinstance(live, dict):
            for key in ("started_at", "last_state_change_at", "last_buffer_at"):
                live_value = live.get(key)
                if live_value is not None:
                    derived[key] = live_value
        return derived
    return _read_air_pipeline_live_blob()


__all__ = ["router", "get_video_latency", "get_air_pipeline_status"]
