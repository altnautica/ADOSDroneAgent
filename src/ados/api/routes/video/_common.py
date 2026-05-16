"""Shared constants, helpers, and Pydantic models for video routes.

Holding these in one private module avoids per-sub-router redeclaration
and keeps the shared mediamtx port numbers / pipeline accessor in one
place where every sub-module imports them.
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any, Literal

import httpx
from pydantic import BaseModel, Field

from ados.api.deps import get_agent_app

# mediamtx default ports — must match the values in mediamtx.py.
_MEDIAMTX_API_PORT = 9997
_MEDIAMTX_WEBRTC_PORT = 8889

# Serializes /api/video/record/{start,stop} so two simultaneous toggles
# from the LCD page and the GCS cannot interleave and leave the
# recorder in an inconsistent state.
_RECORD_LOCK = asyncio.Lock()


class CameraSwitchBody(BaseModel):
    """Body for ``POST /api/video/camera/switch``."""

    role: Literal["primary", "secondary"] = Field(
        ..., description="Camera role to bind the device to."
    )
    device_path: str = Field(
        ...,
        min_length=1,
        description="Filesystem device path of the target camera (e.g. /dev/video0).",
    )


class VideoConfigBody(BaseModel):
    """Body for ``POST /api/video/config``.

    Every field is optional; a request with no fields is a no-op
    that returns the current snapshot. Fields are validated
    individually and applied independently so a partial-update
    request leaves the rest of the config untouched.
    """

    bitrate_kbps: int | None = Field(
        default=None, ge=500, le=12000,
        description="Encoder bitrate in kbps. Restarts the encoder.",
    )
    fec_k: int | None = Field(
        default=None, ge=1, le=64,
        description="Reed-Solomon K (data fragments per FEC block).",
    )
    fec_n: int | None = Field(
        default=None, ge=2, le=128,
        description="Reed-Solomon N (total fragments per FEC block).",
    )
    mcs: int | None = Field(
        default=None, ge=0, le=7,
        description="802.11 MCS index passed to wfb_tx -M.",
    )
    auto: bool | None = Field(
        default=None,
        description="Toggle closed-loop adaptive control.",
    )
    tier_idx: int | None = Field(
        default=None, ge=0, le=8,
        description="Pin a specific tier on the bitrate/FEC ladder. "
                    "Implicitly sets auto=False.",
    )


def _get_video_pipeline():
    """Retrieve the video pipeline from the agent app.

    Returns the pipeline object or None if not initialized.
    """
    app = get_agent_app()
    return app.video_pipeline()


def _empty_recording_block() -> dict[str, Any]:
    return {
        "recording": False,
        "recording_filename": None,
        "recording_started_at": None,
    }


def _recording_block(pipeline: Any) -> dict[str, Any]:
    """Pull recording state from a pipeline + its recorder.

    Tolerates both the production VideoRecorder (which exposes
    ``is_recording`` / ``current_filename`` / ``started_at``) and the
    DemoVideoPipeline (which only exposes ``recording`` and a synthetic
    path). Demo path returns the basename of the synthetic path so the
    LCD page and GCS see a non-null filename when "recording" is on.
    """
    if pipeline is None:
        return _empty_recording_block()

    recorder = getattr(pipeline, "recorder", None)
    if recorder is not None:
        try:
            is_rec = bool(getattr(recorder, "is_recording", recorder.recording))
        except Exception:
            is_rec = False
        if not is_rec:
            return _empty_recording_block()
        filename: str | None
        try:
            filename = recorder.current_filename  # type: ignore[attr-defined]
        except AttributeError:
            current_path = getattr(recorder, "current_path", "") or ""
            filename = Path(current_path).name if current_path else None
        try:
            started_at = recorder.started_at  # type: ignore[attr-defined]
        except AttributeError:
            started_at = None
        return {
            "recording": True,
            "recording_filename": filename or None,
            "recording_started_at": started_at,
        }

    # Demo pipeline path: only the boolean flag and the synthetic path
    # are available.
    is_rec = bool(getattr(pipeline, "recording", False))
    if not is_rec:
        return _empty_recording_block()
    fake_path = getattr(pipeline, "_recording_path", "") or ""
    return {
        "recording": True,
        "recording_filename": Path(fake_path).name if fake_path else None,
        "recording_started_at": None,
    }


async def _probe_mediamtx() -> dict | None:
    """Check if mediamtx is alive by hitting its local API.

    In multi-process mode the VideoPipeline object lives in the
    ados-video service, not in ados-api. The API service therefore
    cannot call pipeline.get_status(). Instead we probe mediamtx's
    REST API at localhost:9997 to determine whether a stream is
    active.

    Returns a small dict with stream metadata or None if mediamtx is
    unreachable / has no active streams.
    """
    try:
        async with httpx.AsyncClient(timeout=2.0) as client:
            resp = await client.get(f"http://127.0.0.1:{_MEDIAMTX_API_PORT}/v3/paths/list")
            if resp.status_code != 200:
                return None
            data = resp.json()
            items = data.get("items", [])
            if not items:
                return None
            path = items[0]
            return {
                "running": True,
                "stream_name": path.get("name", "main"),
                "ready": path.get("ready", False),
                "tracks": path.get("tracks", []),
                "readers": len(path.get("readers", [])),
                "webrtc_port": _MEDIAMTX_WEBRTC_PORT,
            }
    except Exception:
        return None


async def _probe_mediamtx_via_whep() -> dict | None:
    """Liveness probe via the public WHEP endpoint.

    The ground-station MediaMTX (started by ``ados-mediamtx-gs``) puts
    auth on the management API at :9997, so ``_probe_mediamtx()`` fails
    with 401 even while the WHEP surface on :8889 is serving frames.
    Probe the WHEP path instead. A GET on the POST-only WHEP endpoint
    returns 405 when bound — that's the canonical "endpoint exists
    and MediaMTX is up" signal, no credentials needed.

    Returns the same dict shape as ``_probe_mediamtx()`` so callers
    can chain. ``ready`` is set to True optimistically because the
    LCD-side fanout doesn't expose a separate readiness signal at
    this layer.
    """
    try:
        async with httpx.AsyncClient(timeout=2.0) as client:
            resp = await client.get(
                f"http://127.0.0.1:{_MEDIAMTX_WEBRTC_PORT}/main/whep"
            )
            if resp.status_code in (200, 204, 405):
                return {
                    "running": True,
                    "stream_name": "main",
                    "ready": True,
                    "tracks": [],
                    "readers": 0,
                    "webrtc_port": _MEDIAMTX_WEBRTC_PORT,
                }
    except Exception:
        return None
    return None


def mediamtx_whep_alive_sync() -> bool:
    """Synchronous version of the WHEP liveness probe.

    Same signal as ``_probe_mediamtx_via_whep`` but callable from the
    heartbeat builder, which runs sync inside an otherwise-async loop.
    Uses a 1s timeout so a stalled MediaMTX cannot delay heartbeat
    delivery beyond one tick.
    """
    try:
        with httpx.Client(timeout=1.0) as client:
            resp = client.get(
                f"http://127.0.0.1:{_MEDIAMTX_WEBRTC_PORT}/main/whep"
            )
            return resp.status_code in (200, 204, 405)
    except Exception:
        return False


__all__ = [
    "_MEDIAMTX_API_PORT",
    "_MEDIAMTX_WEBRTC_PORT",
    "_RECORD_LOCK",
    "CameraSwitchBody",
    "VideoConfigBody",
    "_get_video_pipeline",
    "_empty_recording_block",
    "_recording_block",
    "_probe_mediamtx",
    "_probe_mediamtx_via_whep",
    "mediamtx_whep_alive_sync",
]
