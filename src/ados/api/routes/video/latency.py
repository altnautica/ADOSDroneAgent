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


__all__ = ["router"]
