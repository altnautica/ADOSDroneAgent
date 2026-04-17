"""Status and telemetry routes."""

from __future__ import annotations

from fastapi import APIRouter, Request

from ados import __version__
from ados.api.deps import get_agent_app

router = APIRouter()


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

    health_info = app.health.last.to_dict()

    from ados.core.deps import check_video_dependencies
    deps = check_video_dependencies()

    # Read live state from StateIPC if available (multi-process mode), fall
    # back to the in-process FC connection if running as single-process.
    state_client = getattr(app, "_state_client", None)
    state = state_client.state if state_client and state_client.state else {}

    fc_connected = state.get("fc_connected")
    fc_port = state.get("fc_port")
    fc_baud = state.get("fc_baud")
    state_uptime = state.get("service_uptime")

    if fc_connected is None and app._fc_connection is not None:
        # Single-process fallback (e.g. running ados-agent monolithically)
        fc_connected = app._fc_connection.connected
        fc_port = getattr(app._fc_connection, "port", None)
        fc_baud = getattr(app._fc_connection, "baud", None)

    if fc_connected is None:
        fc_connected = False

    # Prefer the mavlink service's uptime when available (it's the actual
    # "agent uptime" the user cares about). Falls back to the API service's
    # own uptime which is 0.0 in the StandaloneAgent shim.
    uptime = state_uptime if state_uptime is not None else app.uptime_seconds

    return {
        "version": __version__,
        "uptime_seconds": uptime,
        "board": board_info,
        "health": health_info,
        "fc_connected": fc_connected,
        "fc_port": fc_port,
        "fc_baud": fc_baud,
        "dependencies": {d.name: d.found for d in deps},
    }


@router.get("/status/full")
async def get_full_status(request: Request):
    """Consolidated status: agent info, services, resources, video, telemetry.

    Single endpoint that replaces 4 separate GCS poll requests (/api/status,
    /api/services, /api/system, /api/video) with one round-trip. Cuts polling
    latency from ~300-400ms (4 sequential requests) to ~100ms (1 request).
    """
    import time

    app = get_agent_app()

    # --- Status (same logic as /api/status) ---
    board_info = {}
    try:
        from ados.hal.detect import detect_board
        board = detect_board()
        board_info = board.to_dict()
    except Exception:
        pass

    health_info = app.health.last.to_dict()

    state_client = getattr(app, "_state_client", None)
    state = state_client.state if state_client and state_client.state else {}

    fc_connected = state.get("fc_connected")
    fc_port = state.get("fc_port")
    fc_baud = state.get("fc_baud")
    state_uptime = state.get("service_uptime")

    if fc_connected is None and app._fc_connection is not None:
        fc_connected = app._fc_connection.connected
        fc_port = getattr(app._fc_connection, "port", None)
        fc_baud = getattr(app._fc_connection, "baud", None)

    if fc_connected is None:
        fc_connected = False

    uptime = state_uptime if state_uptime is not None else app.uptime_seconds

    # --- Services (same logic as /api/services) ---
    tracker_data = app.services.to_dict()
    services = []
    task_names = {t.get_name() for t in app._tasks}
    now_mono = time.monotonic()

    for task_item in app._tasks:
        name = task_item.get_name()
        tracked = tracker_data.get(name, {})
        raw_state = tracked.get("state", "running" if not task_item.done() else "stopped")
        svc_entry = {
            "name": name,
            "state": raw_state,
            "task_done": task_item.done(),
        }
        transitions = app.services.get_transitions(name)
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
            transitions = app.services.get_transitions(name)
            svc_uptime = 0.0
            if transitions:
                for ts, st in reversed(transitions):
                    if st.value == "running":
                        svc_uptime = now_mono - ts
                        break
            svc_entry["uptimeSeconds"] = round(svc_uptime)
            services.append(svc_entry)

    # --- System resources (same logic as /api/system) ---
    resources = {}
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

        resources = {
            "cpu_percent": cpu_percent,
            "memory_percent": mem.percent,
            "disk_percent": disk.percent,
            "temperature": next(iter(temps.values()), None),
        }
    except ImportError:
        pass

    # --- Video (same logic as /api/video with mediamtx probe) ---
    from ados.api.routes.video import _get_video_pipeline, _probe_mediamtx

    video = {"state": "not_initialized", "whep_url": None}
    pipeline = _get_video_pipeline()
    if pipeline is not None:
        vid_status = pipeline.get_status()
        if vid_status.get("mediamtx", {}).get("running"):
            host = request.headers.get("host", "localhost").split(":")[0]
            webrtc_port = vid_status["mediamtx"].get("webrtc_port", 8889)
            video = {"state": "running", "whep_url": f"http://{host}:{webrtc_port}/main/whep"}
        else:
            video = {"state": vid_status.get("state", "stopped"), "whep_url": None}
    else:
        mtx = await _probe_mediamtx()
        if mtx and mtx.get("ready"):
            host = request.headers.get("host", "localhost").split(":")[0]
            video = {"state": "running", "whep_url": f"http://{host}:8889/main/whep"}

    # --- Telemetry snapshot ---
    telemetry = {}
    if app._vehicle_state:
        telemetry = app._vehicle_state.to_dict()

    # --- Capabilities (from FeatureManager if available) ---
    capabilities = {}
    fm = getattr(app, "feature_manager", None)
    if fm is not None:
        try:
            capabilities = fm.get_capabilities()
        except Exception:
            pass

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
            # mesh_capable hint from /etc/ados/profile.conf
            try:
                import yaml as _yaml
                pc = _yaml.safe_load(
                    open("/etc/ados/profile.conf", encoding="utf-8")
                ) or {}
                mesh_block["mesh_capable"] = bool(pc.get("mesh_capable", False))
            except OSError:
                mesh_block["mesh_capable"] = False
            if role in ("relay", "receiver"):
                try:
                    import json as _json
                    from pathlib import Path as _P
                    snap = _json.loads(
                        _P("/run/ados/mesh-state.json").read_text(encoding="utf-8")
                    )
                    mesh_block["up"] = bool(snap.get("up", False))
                    mesh_block["peer_count"] = len(snap.get("neighbors", []))
                    mesh_block["selected_gateway"] = snap.get("selected_gateway")
                    mesh_block["partition"] = bool(snap.get("partition", False))
                except (OSError, ValueError):
                    pass
    except Exception:
        pass

    return {
        "version": __version__,
        "uptime_seconds": uptime,
        "board": board_info,
        "health": health_info,
        "fc_connected": fc_connected,
        "fc_port": fc_port,
        "fc_baud": fc_baud,
        "services": services,
        "resources": resources,
        "video": video,
        "telemetry": telemetry,
        "capabilities": capabilities,
        "mesh": mesh_block,
    }


@router.get("/telemetry")
async def get_telemetry():
    """Current vehicle state from VehicleState."""
    app = get_agent_app()
    if app._vehicle_state:
        return app._vehicle_state.to_dict()
    return {}
