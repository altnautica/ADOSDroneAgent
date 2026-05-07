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
