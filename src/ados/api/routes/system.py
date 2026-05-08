"""System resources + supervisor lifecycle routes."""

from __future__ import annotations

import asyncio
import shutil
import subprocess
from typing import Any

from fastapi import APIRouter, BackgroundTasks

router = APIRouter()


@router.get("/system")
async def get_system_resources():
    """Return CPU, memory, disk, and temperature info."""
    try:
        import psutil

        cpu_percent = psutil.cpu_percent(interval=0.1)
        mem = psutil.virtual_memory()
        disk = psutil.disk_usage("/")

        # Temperature (Linux only, best-effort)
        temps = {}
        try:
            for name, entries in psutil.sensors_temperatures().items():
                if entries:
                    temps[name] = entries[0].current
        except (AttributeError, OSError):
            pass

        return {
            "cpu_percent": cpu_percent,
            "cpu_count": psutil.cpu_count(),
            "memory_total_mb": round(mem.total / (1024 * 1024)),
            "memory_used_mb": round(mem.used / (1024 * 1024)),
            "memory_percent": mem.percent,
            "disk_total_gb": round(disk.total / (1024**3), 1),
            "disk_used_gb": round(disk.used / (1024**3), 1),
            "disk_percent": disk.percent,
            "temperatures": temps,
        }
    except ImportError:
        # psutil missing — return null fields so the dashboard can
        # render "—" rather than misleading zeros that look like an
        # idle but live system.
        return {
            "cpu_percent": None,
            "cpu_count": None,
            "memory_total_mb": None,
            "memory_used_mb": None,
            "memory_percent": None,
            "disk_total_gb": None,
            "disk_used_gb": None,
            "disk_percent": None,
            "temperatures": {},
            "available": False,
        }


@router.post("/v1/system/restart-supervisor")
async def post_restart_supervisor(
    background_tasks: BackgroundTasks,
) -> dict[str, Any]:
    """Trigger ``systemctl restart ados-supervisor``.

    The supervisor unit owns the agent process tree, so a restart
    here brings every child (api, video, wfb, ...) back through the
    same lifecycle the install script set up. The HTTP response is
    returned immediately because ``systemctl restart`` blocks until
    the unit settles, and the unit kills the agent process which
    serves this very route. The systemctl call runs as a FastAPI
    background task so the route handler can flush the response
    first.

    The endpoint reports ``ok=True`` when it manages to schedule the
    systemctl call; the actual restart is asynchronous. A False
    result here means the operator cannot launch a restart from this
    surface (no systemctl binary, scheduling rejected, etc.), and
    the caller should surface the error string.
    """
    if shutil.which("systemctl") is None:
        return {
            "ok": False,
            "message": "systemctl binary not found on PATH",
        }

    background_tasks.add_task(_run_supervisor_restart)
    return {
        "ok": True,
        "message": "ados-supervisor restart scheduled",
    }


async def _run_supervisor_restart() -> None:
    """Background-task body that fires ``systemctl restart``.

    A short asyncio sleep gives the HTTP layer a moment to flush the
    JSON response back to the LCD before the supervisor signals this
    process. 200 ms is below the LCD's render tick at 5 Hz so the
    confirm dialog pops cleanly first.
    """
    await asyncio.sleep(0.2)
    try:
        await asyncio.to_thread(
            subprocess.run,
            ["systemctl", "restart", "ados-supervisor"],
            check=False,
            timeout=10,
        )
    except subprocess.SubprocessError:
        # The supervisor restart kills us mid-call; the exception
        # surface here is benign because the unit IS restarting.
        return
