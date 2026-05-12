"""Video pipeline API routes."""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any, Literal

import httpx
from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import FileResponse, Response
from pydantic import BaseModel, Field

from ados.api.deps import get_agent_app
from ados.core.logging import get_logger

log = get_logger("api.video")

router = APIRouter()

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

# mediamtx default ports — must match the values in mediamtx.py.
_MEDIAMTX_API_PORT = 9997
_MEDIAMTX_WEBRTC_PORT = 8889


def _discover_cameras_for_api() -> dict:
    """Run a fresh HAL camera discovery for the API response.

    The live camera_mgr assignment lives in the ados-video process and
    is not directly readable from the API process. Re-running the HAL
    discovery is cheap (~150ms) and gives the operator the same view
    the wizard's hardware-check step shows so the Video step's
    "Detected cameras" panel is not silently empty when a camera IS
    plugged in. Returns the same shape camera_mgr.to_dict() returns
    with assignments left empty (we cannot infer those without IPC).
    """
    try:
        from ados.hal.camera import discover_cameras

        cams = discover_cameras()
        return {
            "cameras": [c.to_dict() for c in cams],
            "assignments": {},
        }
    except Exception:
        return {"cameras": [], "assignments": {}}


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


def _enumerate_cameras(pipeline: Any) -> dict[str, Any]:
    """Return cameras + assignments in the API contract shape.

    When the pipeline is live we read the camera_manager directly so the
    response includes the operator's current role bindings. Otherwise we
    fall back to a fresh HAL discovery (the same fallback used for the
    /api/video status response) and return empty assignments.
    """
    if pipeline is not None:
        cam_mgr = getattr(pipeline, "camera_manager", None)
        if cam_mgr is not None:
            cameras = [
                {
                    "device_path": c.device_path,
                    "type": c.type.value,
                    "label": c.name,
                    "width": c.width,
                    "height": c.height,
                }
                for c in cam_mgr.cameras
            ]
            assignments: dict[str, str] = {}
            for role, cam in cam_mgr.assignments.items():
                assignments[role.value] = cam.device_path
            return {"cameras": cameras, "assignments": assignments}

    # Pipeline not live: fall back to a one-shot HAL discovery so the
    # operator still sees what's plugged in.
    try:
        from ados.hal.camera import discover_cameras

        cams = discover_cameras()
        return {
            "cameras": [
                {
                    "device_path": c.device_path,
                    "type": c.type.value,
                    "label": c.name,
                    "width": c.width,
                    "height": c.height,
                }
                for c in cams
            ],
            "assignments": {},
        }
    except Exception:
        return {"cameras": [], "assignments": {}}


async def _probe_mediamtx() -> dict | None:
    """Check if mediamtx is alive by hitting its local API.

    In multi-process mode the VideoPipeline object lives in the ados-video
    service, not in ados-api.  The API service therefore cannot call
    pipeline.get_status().  Instead we probe mediamtx's REST API at
    localhost:9997 to determine whether a stream is active.

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


@router.get("/video")
async def get_video_status(request: Request):
    """Video pipeline status: cameras, streams, recording, mediamtx, WHEP URL."""
    from ados.core.deps import check_video_dependencies

    deps = check_video_dependencies()
    deps_dict = {d.name: {"found": d.found, "path": d.path} for d in deps}

    pipeline = _get_video_pipeline()

    # Multi-process mode: pipeline is None because ados-video owns it.
    # Probe mediamtx directly to determine video state. The camera list
    # comes from a fresh HAL discovery so the operator sees what the
    # agent thinks is plugged in even though the live camera_mgr
    # assignments live in the ados-video process and are not directly
    # readable from here without IPC.
    if pipeline is None:
        cameras_payload = _discover_cameras_for_api()
        mtx = await _probe_mediamtx()
        recording_block = _empty_recording_block()
        if mtx and mtx.get("ready"):
            host = request.headers.get("host", "localhost").split(":")[0]
            whep_url = f"http://{host}:{_MEDIAMTX_WEBRTC_PORT}/main/whep"
            return {
                "state": "running",
                "cameras": cameras_payload,
                "recorder": {"recording": False, "current_path": "", "recordings_dir": ""},
                "mediamtx": mtx,
                "whep_url": whep_url,
                "dependencies": deps_dict,
                **recording_block,
            }
        return {
            "state": "not_initialized",
            "cameras": cameras_payload,
            "recorder": {"recording": False, "current_path": "", "recordings_dir": ""},
            "mediamtx": {"running": False},
            "whep_url": None,
            "dependencies": deps_dict,
            **recording_block,
        }

    status = pipeline.get_status()

    # Construct WHEP URL from mediamtx state
    if status.get("mediamtx", {}).get("running"):
        webrtc_port = status["mediamtx"].get("webrtc_port", 8889)
        host = request.headers.get("host", "localhost").split(":")[0]
        status["whep_url"] = f"http://{host}:{webrtc_port}/main/whep"
    else:
        status["whep_url"] = None

    status["dependencies"] = deps_dict
    # Surface the recording state at the top level so the LCD video page
    # and the GCS can read it without re-implementing the recorder
    # serializer.
    status.update(_recording_block(pipeline))
    return status


@router.get("/video/snapshot.jpg")
async def get_snapshot_jpg(request: Request) -> Response:
    """Serve the most-recent JPEG snapshot as image/jpeg.

    Used by the dashboard video panel as the final fallback when both
    WebRTC WHEP and HLS playback fail. The endpoint:
    1. Looks for the latest snapshot in the recording dir.
    2. If none exists, captures a fresh one (synchronously).
    3. Returns the bytes with image/jpeg content-type.
    """
    pipeline = _get_video_pipeline()
    app = get_agent_app()

    snapshot_dir: Path | None = None
    try:
        recording_dir = app.config.video.recording.path
        snapshot_dir = Path(recording_dir.rstrip("/")) / "snapshots"
    except Exception:  # noqa: BLE001
        pass

    # Try the latest existing snapshot first — fast path, no camera I/O.
    if snapshot_dir and snapshot_dir.is_dir():
        latest = max(
            (p for p in snapshot_dir.glob("*.jpg")),
            key=lambda p: p.stat().st_mtime,
            default=None,
        )
        if latest is not None and latest.is_file():
            return FileResponse(
                str(latest),
                media_type="image/jpeg",
                headers={"Cache-Control": "no-store"},
            )

    # No cached snapshot — capture a fresh one inline.
    if pipeline is None:
        raise HTTPException(status_code=404, detail="no snapshot available")

    if hasattr(pipeline, "capture_snapshot"):
        result = pipeline.capture_snapshot()
        path = await result if asyncio.iscoroutine(result) else result
        if path and Path(path).is_file():
            return FileResponse(
                path, media_type="image/jpeg",
                headers={"Cache-Control": "no-store"},
            )

    cam_mgr = getattr(pipeline, "camera_manager", None)
    primary = cam_mgr.get_primary() if cam_mgr else None
    if primary is None:
        raise HTTPException(status_code=404, detail="no primary camera")

    from ados.services.video.snapshot import capture_snapshot

    state = app.vehicle_state()
    gps_lat = state.lat if state else 0.0
    gps_lon = state.lon if state else 0.0
    if snapshot_dir is None:
        raise HTTPException(status_code=500, detail="snapshot dir unavailable")
    path = await capture_snapshot(primary, str(snapshot_dir), gps_lat, gps_lon)
    if not path or not Path(path).is_file():
        raise HTTPException(status_code=500, detail="snapshot capture failed")
    return FileResponse(
        path, media_type="image/jpeg", headers={"Cache-Control": "no-store"},
    )


@router.post("/video/snapshot")
async def trigger_snapshot():
    """Capture a JPEG snapshot from the primary camera."""
    pipeline = _get_video_pipeline()
    if pipeline is None:
        return {"error": "video pipeline not initialized", "path": ""}

    # For demo pipelines, use the demo capture method
    if hasattr(pipeline, "capture_snapshot"):
        result = pipeline.capture_snapshot()
        # Handle both sync and async capture methods
        if asyncio.iscoroutine(result):
            path = await result
        else:
            path = result
        if path:
            return {"path": path, "status": "captured"}
        return {"error": "capture failed", "path": ""}

    # For real pipelines, use the snapshot module
    cam_mgr = pipeline.camera_manager
    primary = cam_mgr.get_primary()
    if primary is None:
        return {"error": "no primary camera", "path": ""}

    from ados.services.video.snapshot import capture_snapshot

    app = get_agent_app()
    state = app.vehicle_state()
    gps_lat = state.lat if state else 0.0
    gps_lon = state.lon if state else 0.0

    recording_dir = app.config.video.recording.path
    snapshot_dir = recording_dir.rstrip("/") + "/snapshots"

    path = await capture_snapshot(primary, snapshot_dir, gps_lat, gps_lon)
    if path and Path(path).is_file():
        return {"path": path, "status": "captured"}
    return {"error": "capture failed or file not found", "path": str(path or "")}


@router.post("/video/record/start")
async def start_recording():
    """Start MP4 recording from the primary camera.

    Mutex-guarded so concurrent toggles from the LCD page and the GCS
    cannot race each other into a half-started recorder. The response
    surfaces the live ``recording`` flag in addition to the legacy
    ``path`` / ``status`` keys so callers can update their UI without a
    follow-up ``GET /api/video`` poll.
    """
    pipeline = _get_video_pipeline()
    if pipeline is None:
        return {
            "error": "video pipeline not initialized",
            "path": "",
            **_empty_recording_block(),
        }

    async with _RECORD_LOCK:
        if hasattr(pipeline, "start_recording") and callable(pipeline.start_recording):
            # Demo pipeline has sync start_recording.
            path = pipeline.start_recording()
            return {
                "path": path,
                "status": "recording",
                **_recording_block(pipeline),
            }

        # Real pipeline: use recorder.
        recorder = pipeline.recorder
        if recorder.recording:
            return {
                "path": recorder.current_path,
                "status": "already_recording",
                **_recording_block(pipeline),
            }

        path = await recorder.start_recording()
        if path:
            return {
                "path": path,
                "status": "recording",
                **_recording_block(pipeline),
            }
        return {
            "error": "failed to start recording",
            "path": "",
            **_empty_recording_block(),
        }


@router.post("/video/record/stop")
async def stop_recording():
    """Stop the current MP4 recording.

    Mutex-guarded; see :func:`start_recording`.
    """
    pipeline = _get_video_pipeline()
    if pipeline is None:
        return {
            "error": "video pipeline not initialized",
            "path": "",
            **_empty_recording_block(),
        }

    async with _RECORD_LOCK:
        if hasattr(pipeline, "stop_recording") and callable(pipeline.stop_recording):
            path = pipeline.stop_recording()
            return {
                "path": path,
                "status": "stopped",
                **_recording_block(pipeline),
            }

        recorder = pipeline.recorder
        if not recorder.recording:
            return {
                "error": "no active recording",
                "path": "",
                **_empty_recording_block(),
            }

        path = await recorder.stop_recording()
        return {
            "path": path,
            "status": "stopped",
            **_recording_block(pipeline),
        }


@router.get("/video/cameras")
async def list_cameras():
    """Enumerate cameras + their current role assignments.

    Always returns 200 with at least an empty ``cameras`` array. When the
    video pipeline is live the response reflects camera_manager state so
    the operator's role bindings show through; otherwise we fall back
    to a fresh HAL discovery so a not-yet-running pipeline doesn't make
    the UI look like the SBC has no cameras attached.
    """
    pipeline = _get_video_pipeline()
    return _enumerate_cameras(pipeline)


@router.post("/video/camera/switch")
async def switch_camera(body: CameraSwitchBody):
    """Reassign a camera role and restart the encoder.

    Validates that ``device_path`` matches an enumerated camera before
    the role assignment is persisted. Returns 400 when the device path
    is unknown so the LCD page and the GCS can surface a precise
    rejection reason instead of a vague 500.
    """
    pipeline = _get_video_pipeline()
    if pipeline is None:
        raise HTTPException(
            status_code=503, detail="video pipeline not initialized"
        )

    cam_mgr = getattr(pipeline, "camera_manager", None)
    if cam_mgr is None:
        raise HTTPException(
            status_code=503, detail="camera manager unavailable"
        )

    known_paths = {c.device_path for c in cam_mgr.cameras}
    if body.device_path not in known_paths:
        raise HTTPException(status_code=400, detail="unknown camera")

    # The pipeline owns the lock + serialization; surface its errors as
    # 400 (lookup) or 500 (everything else).
    try:
        await pipeline.restart_with_camera(body.role, body.device_path)
    except LookupError as exc:
        raise HTTPException(status_code=400, detail=str(exc)) from exc
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc)) from exc

    return {"ok": True, "restarting": True}


# Phase 13: in-process GStreamer air pipeline stats. The pipeline runs
# in the ``ados-video`` process and publishes its stats to a
# ``/run/ados/air-pipeline.json`` snapshot at 1 Hz so this endpoint
# in the API process can serve them without IPC. When the snapshot
# file is missing the air pipeline is not in use (legacy bash path
# active) and the endpoint returns a 204 No Content.


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


def _read_state_file(path: str) -> dict[str, Any] | None:
    """Read a JSON snapshot file written by a wfb-side controller.

    The BitrateController and HopSupervisor run inside ados-wfb
    (separate process from ados-api in production multi-process
    systemd). They persist their snapshots to /run/ados/*.json
    every ~5 s; this reader pulls whichever file is asked for.
    Returns None on any read or parse failure so the caller can
    fall back to defaults.
    """
    try:
        from pathlib import Path
        import json

        p = Path(path)
        if not p.is_file():
            return None
        blob = json.loads(p.read_text())
        return blob if isinstance(blob, dict) else None
    except (OSError, ValueError):
        return None


def _bitrate_controller_snapshot(app: Any) -> dict[str, Any] | None:
    """Read the BitrateController snapshot.

    First tries the in-process accessor (single-process mode,
    bench dev). Falls back to the state file the controller
    persists at /run/ados/bitrate-controller.json (production
    multi-process). Returns None when neither path yields data.
    """
    getter = getattr(app, "bitrate_controller", None)
    if callable(getter):
        ctrl = getter()
        if ctrl is not None:
            snap_fn = getattr(ctrl, "snapshot", None)
            if callable(snap_fn):
                try:
                    return snap_fn()
                except Exception:  # noqa: BLE001
                    pass
    from ados.core.paths import BITRATE_CONTROLLER_JSON

    return _read_state_file(str(BITRATE_CONTROLLER_JSON))


def _hop_supervisor_snapshot(app: Any) -> dict[str, Any] | None:
    """Read the HopSupervisor snapshot.

    Same dual-path pattern as the bitrate controller: prefer
    the in-process accessor, fall back to the state file at
    /run/ados/hop-supervisor.json.
    """
    getter = getattr(app, "hop_supervisor", None)
    if callable(getter):
        sup = getter()
        if sup is not None:
            snap_fn = getattr(sup, "snapshot", None)
            if callable(snap_fn):
                try:
                    return snap_fn()
                except Exception:  # noqa: BLE001
                    pass
    from ados.core.paths import HOP_SUPERVISOR_JSON

    return _read_state_file(str(HOP_SUPERVISOR_JSON))


@router.get("/video/config")
async def get_video_config() -> dict[str, Any]:
    """Live snapshot of the adaptive bitrate + FEC + radio config.

    Combines the static wfb config (channel, mcs, fec_k/fec_n
    persisted to /etc/ados/config.yaml) with the dynamic ladder
    state from the BitrateController. Shape is stable enough that
    the GCS Video Link panel can render its sparklines without a
    schema migration when an additional metric is added.
    """
    app = get_agent_app()
    cfg = app.config
    wfb_cfg = getattr(cfg.video, "wfb", None) if cfg is not None else None
    camera_cfg = getattr(cfg.video, "camera", None) if cfg is not None else None

    radio = {
        "channel": getattr(wfb_cfg, "channel", None) if wfb_cfg else None,
        "band": getattr(wfb_cfg, "band", None) if wfb_cfg else None,
        "mcs_index": getattr(wfb_cfg, "mcs_index", None) if wfb_cfg else None,
        "fec_k": getattr(wfb_cfg, "fec_k", None) if wfb_cfg else None,
        "fec_n": getattr(wfb_cfg, "fec_n", None) if wfb_cfg else None,
        "tx_power_dbm": (
            getattr(wfb_cfg, "tx_power_dbm", None) if wfb_cfg else None
        ),
        "preset": getattr(wfb_cfg, "wfb_link_preset", None) if wfb_cfg else None,
    }
    encoder = {
        "bitrate_kbps": (
            getattr(camera_cfg, "bitrate_kbps", None) if camera_cfg else None
        ),
        "width": getattr(camera_cfg, "width", None) if camera_cfg else None,
        "height": getattr(camera_cfg, "height", None) if camera_cfg else None,
        "fps": getattr(camera_cfg, "fps", None) if camera_cfg else None,
        "codec": getattr(camera_cfg, "codec", None) if camera_cfg else None,
    }
    adaptive = {
        "available": (
            getattr(wfb_cfg, "adaptive_bitrate_enabled", False)
            if wfb_cfg else False
        ),
    }
    snap = _bitrate_controller_snapshot(app)
    if snap is not None:
        adaptive.update(snap)

    # Hop supervisor snapshot: drone-side periodic + reactive
    # frequency hopper. Drives the GCS ChannelHistoryChart.
    # Falls back to a minimal stub on incapable rigs (e.g. GS
    # profile, which spawns the listener only, no supervisor)
    # so the GCS chart can render an "armed but quiet" state.
    hop_snap = _hop_supervisor_snapshot(app)
    if hop_snap is None:
        hopping = {
            "enabled": (
                getattr(wfb_cfg, "auto_hop_enabled", False)
                if wfb_cfg else False
            ),
            "band": getattr(wfb_cfg, "band", None) if wfb_cfg else None,
            "hop_period_seconds": (
                getattr(wfb_cfg, "hop_period_seconds", None)
                if wfb_cfg else None
            ),
            "history": [],
            "last_hop_at": 0.0,
        }
    else:
        hopping = hop_snap
    return {
        "radio": radio,
        "encoder": encoder,
        "adaptive": adaptive,
        "hopping": hopping,
    }


@router.post("/video/config")
async def set_video_config(body: VideoConfigBody) -> dict[str, Any]:
    """Apply zero or more video / radio tuning knobs.

    Each field is optional and applied independently. Returns the
    same shape as GET /video/config so the GCS can refresh its
    local state from a single response. Fields that the agent
    couldn't apply (e.g. wfb_manager is None in this process)
    surface in ``warnings`` so a partial success is visible.
    """
    app = get_agent_app()
    wfb_mgr = app.wfb_manager() if hasattr(app, "wfb_manager") else None
    pipeline = app.video_pipeline() if hasattr(app, "video_pipeline") else None
    ctrl_getter = getattr(app, "bitrate_controller", None)
    ctrl = ctrl_getter() if callable(ctrl_getter) else None

    warnings: list[str] = []

    # Bitrate: pipeline-side restart. Skip if pipeline not in process.
    if body.bitrate_kbps is not None:
        if pipeline is not None and hasattr(pipeline, "set_video_bitrate"):
            ok = await pipeline.set_video_bitrate(body.bitrate_kbps)
            if not ok:
                warnings.append("set_video_bitrate_failed")
        else:
            warnings.append("video_pipeline_not_in_process")

    # FEC: stop-then-start wfb_tx. Skip if wfb not in process.
    if body.fec_k is not None or body.fec_n is not None:
        if wfb_mgr is not None and hasattr(wfb_mgr, "set_fec"):
            cfg = app.config.video.wfb
            new_k = body.fec_k if body.fec_k is not None else cfg.fec_k
            new_n = body.fec_n if body.fec_n is not None else cfg.fec_n
            ok = await wfb_mgr.set_fec(new_k, new_n)
            if not ok:
                warnings.append("set_fec_failed")
        else:
            warnings.append("wfb_manager_not_in_process")

    # MCS
    if body.mcs is not None:
        if wfb_mgr is not None and hasattr(wfb_mgr, "set_mcs"):
            ok = await wfb_mgr.set_mcs(body.mcs)
            if not ok:
                warnings.append("set_mcs_failed")
        else:
            warnings.append("wfb_manager_not_in_process")

    # Controller toggles
    if ctrl is not None:
        if body.auto is not None:
            try:
                ctrl.set_auto(body.auto)
            except Exception as exc:  # noqa: BLE001
                warnings.append(f"set_auto_failed:{exc}")
        if body.tier_idx is not None:
            try:
                ok = await ctrl.set_manual_tier(body.tier_idx)
                if not ok:
                    warnings.append("set_manual_tier_failed")
            except Exception as exc:  # noqa: BLE001
                warnings.append(f"set_manual_tier_failed:{exc}")
    elif body.auto is not None or body.tier_idx is not None:
        warnings.append("bitrate_controller_not_in_process")

    response = await get_video_config()
    response["warnings"] = warnings
    return response


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
