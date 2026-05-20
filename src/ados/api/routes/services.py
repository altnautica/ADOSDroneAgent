"""Service status routes."""

from __future__ import annotations

import os
import time

from fastapi import APIRouter

from ados.api.deps import get_agent_app

router = APIRouter()

# Cache process metrics (psutil is expensive to call per-request)
_proc_cache: dict = {"cpu": 0.0, "rss_mb": 0.0, "pid": 0, "ts": 0.0}


def _get_process_metrics() -> dict:
    """Get current process CPU% and RSS memory. Cached for 2 seconds."""
    now = time.monotonic()
    if now - _proc_cache["ts"] < 2.0 and _proc_cache["pid"] == os.getpid():
        return _proc_cache
    try:
        import psutil
        proc = psutil.Process(os.getpid())
        _proc_cache["cpu"] = proc.cpu_percent(interval=0)
        _proc_cache["rss_mb"] = proc.memory_info().rss / (1024 * 1024)
        _proc_cache["pid"] = os.getpid()
        _proc_cache["ts"] = now
    except Exception:
        pass
    return _proc_cache


def _infer_service_state(app, name: str, tracker_state: str, task_done: bool) -> str:
    """Infer true operational state from observable conditions.

    The tracker only knows running/stopped/failed, but many services
    are technically running (asyncio task alive) while functionally
    degraded (e.g. no FC connected, no camera, no WFB adapter).
    """
    if task_done or tracker_state in ("stopped", "failed"):
        return tracker_state

    # FC connection — degraded if no serial port / not connected
    if name == "fc-connection":
        fc = app.fc_connection()
        if fc and not getattr(fc, "connected", False):
            return "degraded"

    # Video pipeline — degraded if mode is disabled or no camera
    if name == "video-pipeline":
        if getattr(app.config.video, "mode", "disabled") == "disabled":
            return "stopped"

    # WFB link — degraded if no compatible adapter found
    if name == "wfb-link":
        wfb = app.wfb_manager()
        if wfb and not getattr(wfb, "has_adapter", False):
            return "degraded"

    # Pairing beacon — idle when already paired
    if name == "pairing-beacon":
        if app.pairing_manager.is_paired:
            return "stopped"

    return tracker_state


@router.get("/services")
async def list_services():
    """List all running services with state machine info and process metrics."""
    app = get_agent_app()

    # Get state machine data from ServiceTracker
    tracker = app.service_tracker
    tracker_data = tracker.to_dict()

    # Get process-level metrics (all services share one process)
    proc = _get_process_metrics()
    pid = proc["pid"]
    total_cpu = proc["cpu"]
    total_rss_mb = proc["rss_mb"]

    # Merge with asyncio task status for runtime info
    services = []
    tasks = app.service_tasks()
    task_names = {t.get_name() for t in tasks}
    running_count = 0

    for task in tasks:
        name = task.get_name()
        tracked = tracker_data.get(name, {})
        raw_state = tracked.get("state", "running" if not task.done() else "stopped")
        state = _infer_service_state(app, name, raw_state, task.done())
        if state == "running":
            running_count += 1
        services.append({
            "name": name,
            "state": state,
            "task_done": task.done(),
            "cancelled": task.cancelled(),
            "last_transition": tracked.get("last_transition", 0),
            "transition_count": tracked.get("transition_count", 0),
        })

    # Include tracked services that might not have an active task
    for name, info in tracker_data.items():
        if name not in task_names:
            state = _infer_service_state(app, name, info["state"], True)
            if state == "running":
                running_count += 1
            services.append({
                "name": name,
                "state": state,
                "task_done": True,
                "cancelled": False,
                "last_transition": info["last_transition"],
                "transition_count": info["transition_count"],
            })

    # Compute per-service uptime from ServiceTracker transitions
    now_mono = time.monotonic()
    for svc in services:
        svc_name = svc["name"]
        transitions = tracker.get_transitions(svc_name)
        svc_uptime = 0.0
        if transitions:
            for ts, st in reversed(transitions):
                if st.value == "running":
                    svc_uptime = now_mono - ts
                    break
        svc["uptimeSeconds"] = round(svc_uptime)

    return {
        "services": services,
        "process": {
            "pid": pid,
            "cpu_percent": round(total_cpu, 1),
            "memory_mb": round(total_rss_mb, 1),
        },
    }


@router.post("/services/{name}/restart")
async def restart_service(name: str):
    """Restart a named systemd service.

    Confirms the restart actually happened by sampling the unit's
    MainPID before and after. PolKit on Debian Bookworm can let
    `systemctl restart` return 0 without actually restarting the unit
    when the caller lacks the right capability — earlier this returned
    `status: ok` to the GCS even when the unit never restarted, so
    operators thought their config change had taken effect when it
    had not.
    """
    import subprocess

    # Validate service name (prevent injection). Includes ground-station
    # profile units so the GS rig's Mission Control Hardware tab can
    # actually restart the receive-side WFB stack.
    allowed = {
        "ados-api",
        "ados-buttons",
        "ados-cloud",
        "ados-cloud-relay",
        "ados-discovery",
        "ados-ethernet",
        "ados-health",
        "ados-hostapd",
        "ados-input",
        "ados-mavlink",
        "ados-mediamtx-gs",
        "ados-mesh-pairing",
        "ados-oled",
        "ados-ota",
        "ados-peripherals",
        "ados-pic",
        "ados-scripting",
        "ados-uplink-router",
        "ados-video",
        "ados-wfb",
        "ados-wfb-rx",
        "ados-wfb-relay",
        "ados-wfb-receiver",
        "ados-wifi-client",
    }
    svc_name = name if name.startswith("ados-") else f"ados-{name}"
    if svc_name not in allowed:
        return {"status": "error", "message": f"Unknown service: {name}"}

    def _main_pid(unit: str) -> int:
        try:
            r = subprocess.run(
                ["systemctl", "show", unit, "-p", "MainPID", "--value"],
                capture_output=True,
                text=True,
                timeout=5,
            )
            return int((r.stdout or "0").strip() or "0")
        except (subprocess.SubprocessError, ValueError):
            return 0

    pid_before = _main_pid(svc_name)

    try:
        result = subprocess.run(
            ["systemctl", "restart", svc_name],
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode != 0:
            msg = result.stderr.strip() or f"Failed to restart {svc_name}"
            return {"status": "error", "message": msg}

        # Give systemd a moment to spawn the new MainPID and confirm
        # the restart actually executed. A no-change PID after
        # returncode=0 means PolKit/permissions silently swallowed the
        # call — surface that as an error so the caller doesn't act on
        # a phantom restart.
        import time
        for _ in range(20):  # up to ~2 s
            time.sleep(0.1)
            pid_after = _main_pid(svc_name)
            if pid_after != 0 and pid_after != pid_before:
                return {
                    "status": "ok",
                    "message": f"Restarted {svc_name}",
                    "pid_before": pid_before,
                    "pid_after": pid_after,
                }
        return {
            "status": "error",
            "message": (
                f"systemctl returned 0 but {svc_name} MainPID did not "
                f"change ({pid_before}). Likely a polkit/permission "
                f"issue — agent may need to run privileged."
            ),
        }
    except subprocess.TimeoutExpired:
        return {"status": "error", "message": f"Restart timed out for {svc_name}"}
    except Exception as exc:
        return {"status": "error", "message": str(exc)}
