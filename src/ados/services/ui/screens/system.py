"""System screen: CPU, RAM, temp, uptime, agent version."""

from __future__ import annotations

from typing import Any


def _fmt_uptime(seconds: Any) -> str:
    if not isinstance(seconds, (int, float)) or seconds < 0:
        return "--"
    s = int(seconds)
    days = s // 86400
    s %= 86400
    hours = s // 3600
    s %= 3600
    minutes = s // 60
    return f"{days}d {hours:02d}:{minutes:02d}"


def render(draw: Any, width: int, height: int, state: dict) -> None:
    sysinfo = state.get("system") or {}
    cpu = sysinfo.get("cpu_pct")
    ram_used = sysinfo.get("ram_used_mb")
    ram_total = sysinfo.get("ram_total_mb")
    temp = sysinfo.get("temp_c")
    uptime = sysinfo.get("uptime_seconds")
    version = sysinfo.get("agent_version") or "--"

    draw.text((0, 0), "SYS", fill="white")
    draw.text((width - 50, 0), f"v{version}"[:10], fill="white")

    cpu_str = f"{cpu}%" if cpu is not None else "--%"
    temp_str = f"{temp}C" if temp is not None else "--C"
    draw.text((0, 14), f"cpu {cpu_str}  t {temp_str}", fill="white")

    if ram_used is not None and ram_total is not None:
        draw.text((0, 30), f"ram {ram_used}/{ram_total}", fill="white")
    else:
        draw.text((0, 30), "ram --/--", fill="white")

    draw.text((0, 46), f"up {_fmt_uptime(uptime)}", fill="white")
