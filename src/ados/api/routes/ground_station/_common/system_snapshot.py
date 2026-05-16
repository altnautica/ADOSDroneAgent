"""System snapshot helpers (CPU / RAM / temp / uptime / agent version).

Both the OLED status schema and the Hardware tab in the GCS read this
shape. Sources are psutil with safe fallbacks so the call always
returns a fully-populated dict even on platforms where one of the
sub-readings throws.
"""

from __future__ import annotations

import time
from typing import Any


def _agent_version() -> str:
    try:
        from ados import __version__ as v

        return str(v)
    except Exception:
        return "unknown"


def _system_snapshot() -> dict[str, Any]:
    """CPU, RAM, temp, uptime from psutil with safe fallbacks."""
    out: dict[str, Any] = {
        "cpu_pct": 0.0,
        "ram_used_mb": 0,
        "ram_total_mb": 0,
        "temp_c": None,
        "uptime_seconds": 0,
        "agent_version": _agent_version(),
    }
    try:
        import psutil

        out["cpu_pct"] = float(psutil.cpu_percent(interval=None))
        vm = psutil.virtual_memory()
        out["ram_used_mb"] = int((vm.total - vm.available) / (1024 * 1024))
        out["ram_total_mb"] = int(vm.total / (1024 * 1024))
        out["uptime_seconds"] = int(time.time() - psutil.boot_time())

        temps_fn = getattr(psutil, "sensors_temperatures", None)
        if callable(temps_fn):
            temps = temps_fn() or {}
            preferred = None
            for key in ("cpu_thermal", "coretemp", "soc_thermal", "k10temp"):
                if key in temps and temps[key]:
                    preferred = temps[key][0]
                    break
            if preferred is None:
                for entries in temps.values():
                    if entries:
                        preferred = entries[0]
                        break
            if preferred is not None and preferred.current is not None:
                out["temp_c"] = float(preferred.current)
    except Exception:
        pass
    return out


__all__ = ["_agent_version", "_system_snapshot"]
