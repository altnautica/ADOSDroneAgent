"""Status and telemetry routes."""

from __future__ import annotations

import asyncio
import hashlib
import json
import subprocess
import time

from fastapi import APIRouter, Request
from fastapi.responses import JSONResponse, Response

from ados import __version__
from ados.api.deps import get_agent_app
from ados.core.cache import status_cache
from ados.core.paths import BOARD_JSON, MESH_STATE_JSON, PROFILE_CONF

router = APIRouter()

# Whether the detected board dict has been persisted to its sidecar this process.
# The board is static per boot, so the write-through runs once on the first status
# read that produces a non-empty board, not on every poll.
_board_persisted = False


def _persist_board_once(board_info: dict) -> None:
    """Persist the detected board dict to its sidecar once per process.

    A separate on-box reader (the native control surface) serves the full board
    block from this file rather than running HAL detect itself. Best-effort and
    atomic (temp sibling + rename), and a no-op for an empty board (a detect that
    raised) or once already written — a failed write never affects the response.
    """
    global _board_persisted
    if _board_persisted or not board_info:
        return
    try:
        BOARD_JSON.parent.mkdir(parents=True, exist_ok=True)
        tmp = BOARD_JSON.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(board_info, sort_keys=True))
        tmp.replace(BOARD_JSON)
        _board_persisted = True
    except OSError:
        pass


def _radio_to_camel(block: dict | None) -> dict | None:
    """Convert a snake_case radio block to the camelCase shape the GCS reads.

    ``build_radio_block`` emits snake_case (the cloud relay remaps it for
    the heartbeat path). The LAN-direct poll has no remapper, so convert
    here once so the consolidated status carries the same canonical shape
    Mission Control's radio normalizer expects.
    """
    if not block:
        return None
    out: dict = {}
    for key, value in block.items():
        head, *tail = key.split("_")
        out[head + "".join(p.title() for p in tail)] = value
    return out


def _gs_video_delivering(wfb_status: dict | None) -> bool:
    """True only when the ground-station WFB link is actually delivering video.

    A reachable WHEP endpoint is not proof of a live downlink: the
    mediamtx ground profile serves WHEP whether or not any frames are
    arriving over the radio, so probing WHEP alone reports "running" with
    zero inbound bytes. The trustworthy signal is the receive link state:
    the stats writer (ados-groundlink wfb_rx) marks ``state == "active"``
    once it is locked and decoding (it does not emit ``"connected"``), and
    ``valid_rx_packets_per_s`` / packet counters confirm frames are still
    flowing right now. Require both — a live link state AND a positive
    valid-decode rate — so a searching / reg-blocked / silent receiver is
    reported as not-delivering. (Operating rule 37: endpoint-reachable is
    never proof of data flowing.)
    """
    if not isinstance(wfb_status, dict):
        return False
    if wfb_status.get("state") not in ("active", "connected"):
        return False
    for key in ("valid_rx_packets_per_s", "packets_received"):
        val = wfb_status.get(key)
        if isinstance(val, (int, float)) and val > 0:
            return True
    return False


# ---------------------------------------------------------------------------
# Cache TTLs for the consolidated status payload.
#
# Keep these short enough that the GCS sees fresh data within a few poll
# cycles. Keep them long enough that the same poller does not refetch on
# every tick. The numbers below assume a 1 to 2 Hz GCS poll rate.
# ---------------------------------------------------------------------------

_VIDEO_DEPS_TTL = 30.0      # external binary lookups change only on package install
_PROFILE_CONF_TTL = 5.0     # profile.conf is rewritten by the operator only
_MESH_STATE_TTL = 1.0       # mesh-state.json is refreshed by the mesh service ~1Hz
_SERVICES_LIST_TTL = 5.0    # service transition snapshot is cheap to read but not free


async def _fetch_video_deps() -> dict[str, bool]:
    """Run the binary-presence scan in a worker thread."""
    from ados.core.deps import check_video_dependencies

    deps = await asyncio.to_thread(check_video_dependencies)
    return {d.name: d.found for d in deps}


async def _fetch_profile_conf() -> dict:
    """Read and parse /etc/ados/profile.conf into a dict."""
    def _read():
        try:
            import yaml as _yaml
            with open(PROFILE_CONF, encoding="utf-8") as fh:
                return _yaml.safe_load(fh) or {}
        except OSError:
            return {}

    return await asyncio.to_thread(_read)


async def _fetch_mesh_state() -> dict:
    """Read and parse /run/ados/mesh-state.json into a dict."""
    def _read():
        try:
            return json.loads(MESH_STATE_JSON.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {}

    return await asyncio.to_thread(_read)


def _build_services_list_sync(app) -> list[dict]:
    """Synchronous services list builder. Pure data, no I/O."""
    tracker = app.service_tracker
    tracker_data = tracker.to_dict()
    services: list[dict] = []
    tasks = app.service_tasks()
    task_names = {t.get_name() for t in tasks}
    now_mono = time.monotonic()

    for task_item in tasks:
        name = task_item.get_name()
        tracked = tracker_data.get(name, {})
        raw_state = tracked.get("state", "running" if not task_item.done() else "stopped")
        svc_entry = {
            "name": name,
            "state": raw_state,
            "task_done": task_item.done(),
        }
        transitions = tracker.get_transitions(name)
        svc_uptime = 0.0
        if transitions:
            for ts, st in reversed(transitions):
                if st.value == "running":
                    svc_uptime = now_mono - ts
                    break
        svc_entry["uptimeSeconds"] = round(svc_uptime)
        services.append(svc_entry)

    for name, info in tracker_data.items():
        if name not in task_names:
            svc_entry = {
                "name": name,
                "state": info["state"],
                "task_done": True,
            }
            transitions = tracker.get_transitions(name)
            svc_uptime = 0.0
            if transitions:
                for ts, st in reversed(transitions):
                    if st.value == "running":
                        svc_uptime = now_mono - ts
                        break
            svc_entry["uptimeSeconds"] = round(svc_uptime)
            services.append(svc_entry)

    if not services:
        services = _systemd_services_fallback()

    # Per-service memory from cgroup accounting. Probed once per distinct
    # owning unit; entries with no unit or a stopped unit land at 0.0.
    # Memoized along with the rest of this list by the 5s status cache.
    _attach_service_memory(services)

    return services


def _attach_service_memory(services: list[dict]) -> None:
    """Add a ``memory_mb`` field to each service entry, in place.

    Mirrors the ``/api/services`` route: resolve each entry's systemd
    unit, read ``MemoryCurrent`` once per distinct unit, write the MiB
    value back. Best-effort and never raises.
    """
    from ados.core.systemd_memory import services_memory_mb, unit_for_service

    unit_by_entry: list[str | None] = [
        unit_for_service(s.get("name", "")) for s in services
    ]
    distinct_units = sorted({u for u in unit_by_entry if u})
    by_unit = services_memory_mb(distinct_units) if distinct_units else {}
    for svc, unit in zip(services, unit_by_entry):
        svc["memory_mb"] = by_unit.get(unit, 0.0) if unit else 0.0


def _systemd_services_fallback() -> list[dict]:
    """Enumerate ados-* systemd units when the in-process tracker is empty.

    The standalone API service runs in its own process under the
    multi-process supervisor. The in-process ServiceTracker only sees
    services managed inside the same Python process, so on that path
    the tracker is empty and the consolidated status reports
    ``services: []`` even when every ``ados-*.service`` is active.

    This fallback shells ``systemctl list-units 'ados-*.service'`` once
    per cache window so the GCS sees the real fleet of running units.
    """
    import shutil

    if not shutil.which("systemctl"):
        return []

    try:
        result = subprocess.run(
            [
                "systemctl",
                "list-units",
                "--type=service",
                "--all",
                "--no-pager",
                "--no-legend",
                "ados-*.service",
            ],
            capture_output=True,
            text=True,
            timeout=3,
        )
    except (OSError, subprocess.SubprocessError):
        return []

    if result.returncode != 0:
        return []

    services: list[dict] = []
    for line in result.stdout.splitlines():
        parts = line.split(None, 4)
        if len(parts) < 4:
            continue
        unit = parts[0].lstrip("●*").strip()
        sub = parts[3].strip()
        if not unit.endswith(".service"):
            continue
        name = unit[: -len(".service")]
        state = "running" if sub == "running" else sub or "unknown"
        services.append(
            {
                "name": name,
                "state": state,
                "status": state,
                "task_done": state != "running",
                "uptimeSeconds": 0,
            }
        )
    return services


def _compute_etag(payload: dict) -> str:
    """Stable short ETag over a JSON-serializable payload."""
    raw = json.dumps(payload, sort_keys=True, default=str).encode("utf-8")
    return hashlib.sha256(raw).hexdigest()[:16]


def _read_camera_status() -> dict[str, object]:
    """Camera presence + USB-recovery state for the LOCAL status surface.

    The cloud heartbeat carries ``cameraState``; the LAN-direct status did not,
    so a locally-paired operator got no camera-missing signal. Reads the same two
    sidecars the video pipeline + supervisor write, staleness-gated, and returns
    only the keys that are fresh and valid (absent otherwise).
    """
    from ados.core.paths import CAMERA_STATE_JSON, CAMERA_USB_RECOVERY_JSON

    out: dict[str, object] = {}
    try:
        camera = json.loads(CAMERA_STATE_JSON.read_text())
    except (OSError, ValueError):
        camera = None
    if isinstance(camera, dict):
        updated = camera.get("updated_at_unix")
        fresh = isinstance(updated, (int, float)) and (
            time.time() - float(updated)
        ) <= 300.0
        state = camera.get("state")
        if fresh and isinstance(state, str) and state in ("ready", "missing", "error"):
            out["cameraState"] = state

    try:
        rec = json.loads(CAMERA_USB_RECOVERY_JSON.read_text())
    except (OSError, ValueError):
        rec = None
    if isinstance(rec, dict):
        updated = rec.get("updated_at_unix")
        fresh = isinstance(updated, (int, float)) and (
            time.time() - float(updated)
        ) <= 60.0
        state = rec.get("camera_usb_recovery_state")
        if fresh and state in (
            "idle",
            "monitoring",
            "rebinding",
            "port_cycling",
            "hub_resetting",
            "needs_hub_reset",
            "guard_blocked",
            "exhausted",
        ):
            out["cameraUsbRecovery"] = {
                "state": state,
                "case": rec.get("case"),
                "attempts": rec.get("attempts", 0),
                "maxAttempts": rec.get("max_attempts", 0),
                "cameraPresent": bool(rec.get("camera_present", False)),
                "expected": bool(rec.get("expected", False)),
                "pppsCapable": bool(rec.get("ppps_capable", False)),
            }
    return out


@router.get("/status")
async def get_status():
    """Agent status: version, uptime, board, FC connection state.

    Under the multi-process supervisor (the normal production path), the
    API service is a separate process from ados-mavlink and has no direct
    access to the FC connection. The `_StandaloneAgent` shim in
    services/api/__main__.py keeps `_fc_connection` as None, so the
    endpoint reads the StateIPC client instead (the mavlink service
    publishes `fc_connected`, `fc_port`, `fc_baud`, and `service_uptime`
    alongside the vehicle state dict at 10Hz on `/run/ados/state.sock`).
    """
    app = get_agent_app()
    board_info = {}
    try:
        from ados.hal.detect import detect_board
        board = detect_board()
        board_info = board.to_dict()
    except Exception:
        pass

    # Persist the board dict once so the native control surface can serve the same
    # block from the sidecar without its own HAL-detect port.
    _persist_board_once(board_info)

    health_info = app.health_dict()

    deps_map = await status_cache.get(
        "video_deps", _fetch_video_deps, _VIDEO_DEPS_TTL,
    )

    fc_status = app.fc_status()
    uptime = (
        fc_status.uptime_seconds
        if fc_status.uptime_seconds is not None
        else app.uptime_seconds()
    )

    return {
        "version": __version__,
        "uptime_seconds": uptime,
        "board": board_info,
        "health": health_info,
        "fc_connected": fc_status.connected,
        "fc_port": fc_status.port,
        "fc_baud": fc_status.baud,
        "dependencies": deps_map,
        **_read_camera_status(),
    }


@router.get("/status/full")
async def get_full_status(request: Request):
    """Consolidated status: agent info, services, resources, video, telemetry.

    Single endpoint that replaces 4 separate GCS poll requests (/api/status,
    /api/services, /api/system, /api/video) with one round-trip. Cuts polling
    latency from ~300-400ms (4 sequential requests) to ~100ms (1 request).

    Per-component TTL caches memoize the expensive lookups (video deps,
    profile YAML, mesh state, services list). Volatile fields (telemetry,
    resources, FC connection state) are recomputed every call. Responses
    carry an ETag and honor `If-None-Match` for 304 replies.
    """
    app = get_agent_app()

    # --- Status (same logic as /api/status) ---
    board_info = {}
    try:
        from ados.hal.detect import detect_board
        board = detect_board()
        board_info = board.to_dict()
    except Exception:
        pass

    # Persist the board dict once so the native control surface can serve the same
    # block from the sidecar without its own HAL-detect port.
    _persist_board_once(board_info)

    health_info = app.health_dict()

    fc_status = app.fc_status()
    uptime = (
        fc_status.uptime_seconds
        if fc_status.uptime_seconds is not None
        else app.uptime_seconds()
    )

    # --- Services (same logic as /api/services), cached briefly ---
    async def _build_services():
        return _build_services_list_sync(app)

    services = await status_cache.get(
        "services_list", _build_services, _SERVICES_LIST_TTL,
    )

    # --- System resources (same logic as /api/system) ---
    # Primary source is the logging store's hardware snapshots (one sampler, the
    # Rust collector); fall back to a live psutil read when the store is
    # unreachable or has not yet captured the essential fields.
    resources = {}
    from ados.api.telemetry_source import derive_resources, latest_hw_signals

    _signals = await latest_hw_signals()
    _res = derive_resources(_signals) if _signals is not None else None
    if _res is not None:
        resources = {
            "cpu_percent": _res["cpu_percent"],
            "memory_percent": _res["memory_percent"],
            "memory_used_mb": _res["memory_used_mb"],
            "memory_total_mb": _res["memory_total_mb"],
            "memory_available_mb": _res["memory_available_mb"],
            "memory_cache_mb": _res["memory_cache_mb"],
            "swap_total_mb": _res["swap_total_mb"],
            "swap_used_mb": _res["swap_used_mb"],
            "swap_percent": _res["swap_percent"],
            "disk_percent": _res["disk_percent"],
            "disk_used_gb": _res["disk_used_gb"],
            "disk_total_gb": _res["disk_total_gb"],
            "temperature": _res["temperature"],
        }
    else:
        try:
            import psutil

            cpu_percent = psutil.cpu_percent(interval=0)
            mem = psutil.virtual_memory()
            disk = psutil.disk_usage("/")
            temps = {}
            try:
                for tname, entries in psutil.sensors_temperatures().items():
                    if entries:
                        temps[tname] = entries[0].current
            except (AttributeError, OSError):
                pass

            swap = psutil.swap_memory()
            # cached + buffers is Linux-only; absent on a dev host, so guard
            # with getattr and fall back to 0 so the field is always present.
            mem_cache_bytes = getattr(mem, "cached", 0) + getattr(mem, "buffers", 0)

            resources = {
                "cpu_percent": cpu_percent,
                "memory_percent": mem.percent,
                "memory_used_mb": round(mem.used / (1024 * 1024)),
                "memory_total_mb": round(mem.total / (1024 * 1024)),
                "memory_available_mb": round(mem.available / (1024 * 1024)),
                "memory_cache_mb": round(mem_cache_bytes / (1024 * 1024)),
                "swap_total_mb": round(swap.total / (1024 * 1024)),
                "swap_used_mb": round(swap.used / (1024 * 1024)),
                "swap_percent": swap.percent,
                "disk_percent": disk.percent,
                "disk_used_gb": round(disk.used / (1024 * 1024 * 1024), 1),
                "disk_total_gb": round(disk.total / (1024 * 1024 * 1024), 1),
                "temperature": next(iter(temps.values()), None),
            }
        except ImportError:
            pass

    # --- WFB radio snapshot, read once and reused below ---
    # Build the wfb status dict here (once) so both the ground-station
    # video gate and the radio block read the same view. On the GS the
    # WfbRxManager lives in a sibling process, so read the shared stats
    # file the same way /api/wfb does; on the drone the in-process
    # manager answers directly.
    wfb_status: dict | None = None
    try:
        wfb_mgr = app.wfb_manager()
        if wfb_mgr is not None:
            wfb_status = wfb_mgr.get_status()
        else:
            from ados.api.routes.wfb import _build_status_from_stats_file

            _wfb_cfg = getattr(getattr(app.config, "video", None), "wfb", None)
            wfb_status = _build_status_from_stats_file(_wfb_cfg)
    except Exception:
        wfb_status = None

    # --- Video (same logic as /api/video with mediamtx probe) ---
    from ados.api.routes.video import (
        _empty_recording_block,
        _probe_mediamtx,
        _probe_mediamtx_via_whep,
        _recording_block,
    )

    # Resolve the profile up front: the video block needs it to tell a drone
    # (whose local mediamtx serves its own camera regardless of the WFB link)
    # apart from a ground-station (whose video only exists when the receive
    # link delivers). Reused below for the payload's profile/role fields.
    from ados.core.profile import current_profile_and_role

    resolved_profile, resolved_role = current_profile_and_role(app.config)

    video: dict = {"state": "not_initialized", "whep_url": None, **_empty_recording_block()}
    pipeline = app.video_pipeline()
    if pipeline is not None:
        # Drone path: the in-process pipeline owns the encoder + mediamtx.
        # Its own state is authoritative; do not gate on the WFB receive
        # link (the drone transmits, it does not receive video).
        vid_status = pipeline.get_status()
        if vid_status.get("mediamtx", {}).get("running"):
            host = request.headers.get("host", "localhost").split(":")[0]
            webrtc_port = vid_status["mediamtx"].get("webrtc_port", 8889)
            video = {
                "state": "running",
                "whep_url": f"http://{host}:{webrtc_port}/main/whep",
                **_recording_block(pipeline),
            }
        else:
            video = {
                "state": vid_status.get("state", "stopped"),
                "whep_url": None,
                **_recording_block(pipeline),
            }
    else:
        # No in-process pipeline (the normal multi-process case — the API
        # process does not own the encoder). Branch on the resolved profile,
        # NOT on pipeline presence: a drone here is a drone, not a GS.
        host = request.headers.get("host", "localhost").split(":")[0]
        if resolved_profile == "drone":
            # Multi-process drone: the drone's local mediamtx still serves its
            # own camera on /main, available regardless of the WFB downlink
            # (a drone transmits, it does not receive video). Probe readiness
            # for the truth — the same signal the setup CLI's _video_access
            # uses — NOT the WFB receive link. (Without this, a drone fell into
            # the GS branch below, whose receive-link gate is always false on a
            # transmit-only node, so it reported stopped/null and the GCS
            # showed "No video" over a perfectly good stream.)
            mtx = await _probe_mediamtx()
            if mtx is None or not mtx.get("ready"):
                mtx = await _probe_mediamtx_via_whep() or mtx
            if mtx and mtx.get("ready"):
                video = {
                    "state": "running",
                    "whep_url": f"http://{host}:8889/main/whep",
                    **_empty_recording_block(),
                }
            # else: leave the default not_initialized (camera not streaming).
        # Ground-station path: mediamtx serves WHEP whether or not frames are
        # arriving over the radio, so a reachable WHEP endpoint is NOT proof of
        # a live downlink. Gate "running" on the WFB receive link actually
        # delivering video (connected + valid decodes), not WHEP reachability.
        # When the link is not delivering, report a non-running state with a
        # null whep_url so the GCS does not show "Video: Live" over a dead
        # radio. (Operating rule 37.)
        elif _gs_video_delivering(wfb_status):
            mtx = await _probe_mediamtx()
            if mtx is None or not mtx.get("ready"):
                # mediamtx-gs puts auth on the management API; the WHEP
                # probe doesn't. Confirm WHEP is serving the live stream.
                mtx = await _probe_mediamtx_via_whep() or mtx
            if mtx and mtx.get("ready"):
                video = {
                    "state": "running",
                    "whep_url": f"http://{host}:8889/main/whep",
                    **_empty_recording_block(),
                }
            else:
                # Link delivering frames but WHEP not yet serving — still
                # coming up rather than live.
                video = {
                    "state": "connecting",
                    "whep_url": None,
                    **_empty_recording_block(),
                }
        else:
            # No live downlink. Report stopped with no playable endpoint.
            video = {
                "state": "stopped",
                "whep_url": None,
                **_empty_recording_block(),
            }

    # --- Telemetry snapshot ---
    telemetry = app.vehicle_state_dict()

    # Capabilities block. Per-agent feature catalogs were retired with
    # the legacy Features tab; capability data now comes from the heartbeat
    # peripherals + HAL profile + plugin manifests, which the GCS reads
    # directly. The field stays in the payload as an empty dict for
    # forward-compat with older Mission Control builds that still read it.
    capabilities: dict = {}

    # --- Mesh snapshot. Only populated on ground-station profile with
    # a non-direct role. Direct nodes and drone-profile nodes get an
    # empty dict so clients can feature-detect cheaply. ---
    mesh_block: dict = {}
    try:
        profile = getattr(app.config.agent, "profile", "auto")
        if profile == "ground_station":
            from ados.services.ground_station.role_manager import get_current_role
            role = get_current_role()
            mesh_block["role"] = role
            # mesh_capable hint from /etc/ados/profile.conf, cached.
            pc = await status_cache.get(
                "profile_conf", _fetch_profile_conf, _PROFILE_CONF_TTL,
            )
            mesh_block["mesh_capable"] = bool(pc.get("mesh_capable", False))
            if role in ("relay", "receiver"):
                snap = await status_cache.get(
                    "mesh_state", _fetch_mesh_state, _MESH_STATE_TTL,
                )
                if snap:
                    mesh_block["up"] = bool(snap.get("up", False))
                    mesh_block["peer_count"] = len(snap.get("neighbors", []))
                    mesh_block["selected_gateway"] = snap.get("selected_gateway")
                    mesh_block["partition"] = bool(snap.get("partition", False))
    except Exception:
        pass

    # resolved_profile / resolved_role were resolved up front (above the video
    # block) via current_profile_and_role — the same helper that drives the
    # cloud heartbeat in services/cloud/__main__.py. Hyphen-form
    # (`"ground-station"`) so the GCS receives the canonical wire shape from
    # both the cloud relay and the direct LAN poll, instead of having to infer
    # profile from `fc_connected`.

    # Native-vs-packaged runtime mode, scoped to the resolved profile, so
    # the LAN-direct poll surfaces the same per-node badge the cloud
    # heartbeat carries. Best-effort: a resolver failure leaves the field
    # at the safe default.
    try:
        from ados.core.runtime_mode import compute_runtime_mode
        runtime_mode = compute_runtime_mode(resolved_profile)
    except Exception:
        runtime_mode = "packaged"

    # --- Radio (WFB link block) — surfaced so the LAN-direct GCS path
    # populates the same radio snapshot the cloud heartbeat carries,
    # including the receive-side metrics (SNR, noise, loss, MCS,
    # receive-liveness). Reuses the wfb_status read once above, so the
    # video gate and the radio block can never disagree about the link.
    radio_block: dict | None = None
    try:
        from ados.core.radio_block import build_radio_block

        radio_block = _radio_to_camel(build_radio_block(wfb_status))
    except Exception:
        radio_block = None

    payload = {
        "version": __version__,
        "uptime_seconds": uptime,
        "board": board_info,
        "health": health_info,
        "fc_connected": fc_status.connected,
        "fc_port": fc_status.port,
        "fc_baud": fc_status.baud,
        "services": services,
        "resources": resources,
        "video": video,
        "telemetry": telemetry,
        "capabilities": capabilities,
        "mesh": mesh_block,
        "radio": radio_block,
        "profile": resolved_profile,
        "role": resolved_role,
        "runtimeMode": runtime_mode,
        **_read_camera_status(),
    }

    etag = _compute_etag(payload)
    if_none_match = request.headers.get("if-none-match")
    if if_none_match and if_none_match.strip('"') == etag:
        return Response(status_code=304, headers={"ETag": f'"{etag}"'})

    return JSONResponse(content=payload, headers={"ETag": f'"{etag}"'})


@router.get("/telemetry")
async def get_telemetry():
    """Current vehicle state from VehicleState."""
    app = get_agent_app()
    return app.vehicle_state_dict()
