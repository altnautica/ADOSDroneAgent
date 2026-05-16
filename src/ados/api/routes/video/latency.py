"""Latency + air-pipeline status routes.

* ``GET /video/latency`` — most-recent SEI-probe glass-to-glass latency
  written by the LCD-side local tap.
* ``GET /v1/video/air-pipeline`` — live stats snapshot from the
  air-side GStreamer pipeline (written by ``ados-video`` at 1 Hz).
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException
from fastapi.responses import Response

from ados.core.logging import get_logger

log = get_logger("api.video")

router = APIRouter()


@router.get("/video/latency")
async def get_video_latency() -> dict[str, Any]:
    """Return the most recent SEI-probe glass-to-glass latency.

    Reads from the state file written by the LCD-side local tap
    when the SEI latency feature is enabled
    (WfbConfig.sei_latency = true). Returns latency_ms=None when
    the probe is disabled or no SEI samples have arrived yet.
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


@router.get("/v1/video/air-pipeline")
async def get_air_pipeline_status():
    """Return the air-side GStreamer pipeline's live stats snapshot.

    Reads the same ``/run/ados/air-pipeline.json`` the heartbeat
    enricher reads. Returns 204 when the air pipeline is not in use
    (legacy bash air pipeline owns the stream).
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


__all__ = ["router", "get_video_latency", "get_air_pipeline_status"]
