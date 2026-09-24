"""Shared constants, helpers, and Pydantic models for video routes.

The encoder, recorder and camera roles all live in the native ``ados-video``
service; this process only probes mediamtx and reads what is on disk, so the
helpers here are those probes plus the tuning-route body model.
"""

from __future__ import annotations

import logging
from typing import Literal

import httpx
from pydantic import BaseModel, Field

# httpx logs every request at INFO. These mediamtx health probes fire on every
# dashboard/heartbeat poll against loopback, so at INFO they flood the journal
# with `GET http://127.0.0.1:8889/main/whep "405 ..."` lines that look like
# errors but are the normal bound-endpoint signal. Lift the threshold to WARNING
# so only genuine httpx problems surface; the probe results are reported through
# the video state, not the request log.
logging.getLogger("httpx").setLevel(logging.WARNING)

# mediamtx default ports — must match the values in mediamtx.py.
_MEDIAMTX_API_PORT = 9997
_MEDIAMTX_WEBRTC_PORT = 8889


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
    preset: Literal["conservative", "balanced", "aggressive"] | None = Field(
        default=None,
        description="Apply a named link preset's (mcs, fec_k, fec_n) trio. "
                    "Leaves adaptive control as-is.",
    )


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

    Returns the same dict shape as ``_probe_mediamtx()``. Crucially a
    405 means BOUND, not STREAMING — the endpoint exists but this probe
    cannot tell whether a publisher is actually delivering frames. So
    ``ready`` is False (degraded, not ready); ``running`` is True. The
    authoritative readiness is the :9997 paths-list (``ready &&
    source``); this whep probe is only the fallback when :9997 is
    auth-blocked, and it must not over-claim a ready stream.
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
                    # Bound, not proven-streaming: do not claim ready off a 405.
                    "ready": False,
                    "tracks": [],
                    "readers": 0,
                    "webrtc_port": _MEDIAMTX_WEBRTC_PORT,
                }
    except Exception:
        return None
    return None


async def mediamtx_ready() -> tuple[bool, dict | None]:
    """Authoritative video-readiness verdict + live track info.

    The :9997 paths-list is the readiness signal: a publisher is really
    delivering only when the ``main`` path reports ``ready`` with a
    ``source``. When :9997 is unreachable or auth-blocked (the ground station
    gates its management API) the verdict is not-ready: a bound WHEP endpoint
    proves mediamtx is up, never that frames flow, so it is not consulted.

    Returns ``(ready, track_info)`` where ``track_info`` carries the codec and
    received-bytes counter of the live path, or ``None`` when no path was read.
    """
    try:
        async with httpx.AsyncClient(timeout=1.0) as client:
            resp = await client.get(
                f"http://127.0.0.1:{_MEDIAMTX_API_PORT}/v3/paths/list"
            )
    except Exception:  # noqa: BLE001 — any transport failure is "not ready"
        return False, None
    if resp.status_code != 200:
        return False, None
    try:
        data = resp.json()
    except ValueError:
        return False, None
    path = _main_path_from_list(data)
    if path is None:
        # 200 but no path yet — mediamtx is up, nothing publishing.
        return False, None
    ready = bool(path.get("ready", False)) and bool(path.get("source"))
    return ready, _track_info_from_path(path)


def _main_path_from_list(data: object) -> dict | None:
    """Pull the ``main`` path object out of a :9997 ``/v3/paths/list`` body,
    falling back to the first path. ``None`` on a malformed body."""
    if not isinstance(data, dict):
        return None
    items = data.get("items", [])
    if not isinstance(items, list) or not items:
        return None
    path = next(
        (p for p in items if isinstance(p, dict) and p.get("name") == "main"),
        items[0] if isinstance(items[0], dict) else None,
    )
    return path if isinstance(path, dict) else None


def _track_info_from_path(path: dict) -> dict | None:
    """Project the codec/bitrate enrichment fields out of a paths-list path
    object. ``None`` when no track is present."""
    tracks = path.get("tracks") or []
    if not isinstance(tracks, list):
        tracks = []
    out: dict[str, object] = {
        "bytes_received": path.get("bytesReceived"),
        "ready": bool(path.get("ready", False)),
    }
    for track in tracks:
        if isinstance(track, str) and track.strip() and "codec" not in out:
            out["codec"] = track.strip()
    return out or None


__all__ = [
    "_MEDIAMTX_API_PORT",
    "_MEDIAMTX_WEBRTC_PORT",
    "VideoConfigBody",
    "_probe_mediamtx",
    "_probe_mediamtx_via_whep",
    "mediamtx_ready",
]
