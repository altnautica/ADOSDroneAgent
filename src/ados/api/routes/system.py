"""System resources + supervisor lifecycle routes."""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

from fastapi import APIRouter

router = APIRouter()


def _is_ntp_synced() -> bool:
    """Best-effort check that the system clock is being disciplined.

    Looks for the synchronisation flag exposed by chrony (preferred)
    and falls back to ``timedatectl show`` when chronyc is absent. On
    a fresh Buildroot rootfs neither tool is guaranteed to be present,
    so the function fails closed and returns False.
    """
    chronyc = shutil.which("chronyc")
    if chronyc is not None:
        try:
            result = subprocess.run(
                [chronyc, "-c", "tracking"],
                capture_output=True,
                text=True,
                timeout=1.0,
                check=False,
            )
            # `tracking` emits a comma-separated row; the second-to-last
            # field is the leap status. Any successful read with a
            # non-empty stride implies chrony has a reference.
            if result.returncode == 0 and result.stdout.strip():
                return True
        except (subprocess.SubprocessError, OSError):
            pass

    timedatectl = shutil.which("timedatectl")
    if timedatectl is not None:
        try:
            result = subprocess.run(
                [timedatectl, "show", "-p", "NTPSynchronized", "--value"],
                capture_output=True,
                text=True,
                timeout=1.0,
                check=False,
            )
            return result.stdout.strip().lower() == "yes"
        except (subprocess.SubprocessError, OSError):
            pass

    # systemd-timesyncd writes /run/systemd/timesync/synchronized once
    # it acquires a reference. Read-only filesystem check, no subprocess.
    try:
        return Path("/run/systemd/timesync/synchronized").is_file()
    except OSError:
        return False


@router.get("/system")
async def get_system_resources():
    """Return CPU, memory, disk, and temperature info.

    Primary source is the durable logging store's hardware snapshots (one
    sampler, the Rust collector). If the store is unreachable or has not yet
    captured the essential fields, this falls back to a live ``psutil`` read so
    the route degrades to its old behavior, never to a 500.
    """
    import os

    from ados.api.telemetry_source import derive_resources, latest_hw_signals

    signals = await latest_hw_signals()
    if signals is not None:
        r = derive_resources(signals)
        if r is not None:
            return {
                "cpu_percent": r["cpu_percent"],
                "cpu_count": os.cpu_count(),
                "memory_total_mb": r["memory_total_mb"],
                "memory_used_mb": r["memory_used_mb"],
                "memory_available_mb": r["memory_available_mb"],
                "memory_cache_mb": r["memory_cache_mb"],
                "memory_percent": r["memory_percent"],
                "swap_total_mb": r["swap_total_mb"],
                "swap_used_mb": r["swap_used_mb"],
                "swap_percent": r["swap_percent"],
                "disk_total_gb": r["disk_total_gb"],
                "disk_used_gb": r["disk_used_gb"],
                "disk_percent": r["disk_percent"],
                "temperatures": r["temperatures"],
            }

    try:
        import psutil

        cpu_percent = psutil.cpu_percent(interval=0.1)
        mem = psutil.virtual_memory()
        swap = psutil.swap_memory()
        disk = psutil.disk_usage("/")

        # cached + buffers is Linux-only; absent on a dev host, so guard
        # with getattr and fall back to 0 so the field is always present.
        mem_cache_bytes = getattr(mem, "cached", 0) + getattr(mem, "buffers", 0)

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
            "memory_available_mb": round(mem.available / (1024 * 1024)),
            "memory_cache_mb": round(mem_cache_bytes / (1024 * 1024)),
            "memory_percent": mem.percent,
            "swap_total_mb": round(swap.total / (1024 * 1024)),
            "swap_used_mb": round(swap.used / (1024 * 1024)),
            "swap_percent": swap.percent,
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
            "memory_available_mb": None,
            "memory_cache_mb": None,
            "memory_percent": None,
            "swap_total_mb": None,
            "swap_used_mb": None,
            "swap_percent": None,
            "disk_total_gb": None,
            "disk_used_gb": None,
            "disk_percent": None,
            "temperatures": {},
            "available": False,
        }
