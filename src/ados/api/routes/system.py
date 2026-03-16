"""System resources routes."""

from __future__ import annotations

from fastapi import APIRouter

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
        return {
            "cpu_percent": 0,
            "cpu_count": 1,
            "memory_total_mb": 0,
            "memory_used_mb": 0,
            "memory_percent": 0,
            "disk_total_gb": 0,
            "disk_used_gb": 0,
            "disk_percent": 0,
            "temperatures": {},
        }
