"""Diagnostics endpoint consumed by the LCD Diagnostics drilldown.

This composes a single read-mostly snapshot of the agent's runtime
identity, the system the agent is running on, and the last few lines
of the ``ados-agent`` journal so the operator can do triage from the
panel without a phone or laptop. The Mission Control GCS surfaces the
same fields through its remote-display pane.

The composition is split into small helper functions so the route
itself stays a thin orchestrator:

* :func:`_collect_agent` — version + uptime + process metrics.
* :func:`_collect_board` — :func:`hal.detect.detect_board` summary.
* :func:`_collect_system` — psutil snapshots of CPU / RAM / disk /
  load avg + a best-effort temperature read.
* :func:`_collect_network` — primary IPv4 + ethernet/wlan MAC reads.
* :func:`_collect_logs` — last N lines from ``journalctl -u ados-agent``.

A 1-second TTL cache wraps the route so repeated polls (LCD + GCS at
the same time) don't fan out to journalctl + psutil simultaneously.
"""

from __future__ import annotations

import asyncio
import os
import re
import subprocess
import time
from pathlib import Path
from typing import Any

from fastapi import APIRouter

from ados import __version__
from ados.api.deps import get_agent_app

router = APIRouter(tags=["diagnostics"])

# Cache the last response for one second. The expensive bits are the
# subprocess call to journalctl and ``psutil.cpu_percent`` which
# samples for 100 ms; both are fine at 1 Hz.
_CACHE_TTL_S = 1.0
_cache_value: dict[str, Any] | None = None
_cache_at: float = 0.0
_cache_lock = asyncio.Lock()


@router.get("/v1/diagnostics")
async def get_diagnostics() -> dict[str, Any]:
    """Return the full diagnostics snapshot for the LCD panel + GCS."""
    global _cache_value, _cache_at
    async with _cache_lock:
        now = time.monotonic()
        if _cache_value is not None and (now - _cache_at) < _CACHE_TTL_S:
            return _cache_value
        snapshot = await _build_snapshot()
        _cache_value = snapshot
        _cache_at = now
        return snapshot


async def _build_snapshot() -> dict[str, Any]:
    """Compose the full diagnostics payload from the helper sections."""
    return {
        "agent": _collect_agent(),
        "board": _collect_board(),
        "system": await _collect_system(),
        "network": _collect_network(),
        "device": _collect_device(),
        "logs": {"agent": await _collect_logs("ados-agent", count=10)},
    }


def _collect_agent() -> dict[str, Any]:
    """Agent identity + process metrics."""
    agent: dict[str, Any] = {"version": __version__}
    # Uptime — prefer the runtime-tracked value, fall back to the
    # process create-time math.
    try:
        runtime = get_agent_app()
        uptime = float(runtime.uptime_seconds)
        if uptime >= 0:
            agent["uptime_seconds"] = uptime
    except Exception:
        agent.setdefault("uptime_seconds", _process_uptime_seconds())
    if "uptime_seconds" not in agent:
        agent["uptime_seconds"] = _process_uptime_seconds()
    # Per-process CPU + RSS.
    try:
        import psutil

        proc = psutil.Process(os.getpid())
        agent["process_cpu_percent"] = float(proc.cpu_percent(interval=0.0))
        agent["process_memory_mb"] = round(
            proc.memory_info().rss / (1024 * 1024), 1,
        )
    except Exception:
        agent.setdefault("process_cpu_percent", None)
        agent.setdefault("process_memory_mb", None)
    return agent


def _process_uptime_seconds() -> float:
    """Return current-process uptime in seconds, or 0.0 on miss."""
    try:
        import psutil

        proc = psutil.Process(os.getpid())
        return max(0.0, time.time() - proc.create_time())
    except Exception:
        return 0.0


def _collect_board() -> dict[str, Any]:
    """Board identity sourced from :func:`hal.detect.detect_board`."""
    try:
        from ados.hal.detect import detect_board

        board = detect_board()
        return {
            "name": getattr(board, "name", None) or "--",
            "soc": getattr(board, "soc", None) or "unknown",
            "arch": getattr(board, "arch", None) or "unknown",
            "ram_total_mb": int(getattr(board, "ram_mb", 0) or 0),
        }
    except Exception:
        return {"name": "--", "soc": "unknown", "arch": "unknown", "ram_total_mb": 0}


async def _collect_system() -> dict[str, Any]:
    """System metrics (CPU / RAM / disk / temp / load avg).

    Primary source is the logging store's hardware snapshots (the Rust collector
    is the single sampler); falls back to a live ``psutil`` read when the store is
    unreachable or has not yet captured the essential fields.
    """
    from ados.api.telemetry_source import derive_resources, latest_hw_signals

    signals = await latest_hw_signals()
    if signals is not None:
        r = derive_resources(signals)
        if r is not None:
            load = r["load_avg"]
            if len(load) != 3:
                try:
                    load = list(os.getloadavg())
                except (AttributeError, OSError):
                    load = [0.0, 0.0, 0.0]
            return {
                "cpu_percent": r["cpu_percent"],
                "memory_used_mb": int(r["memory_used_mb"]),
                "memory_total_mb": int(r["memory_total_mb"]),
                "disk_used_gb": r["disk_used_gb"],
                "disk_total_gb": r["disk_total_gb"],
                "temp_c": r["temperature"]
                if r["temperature"] is not None
                else _read_cpu_temp(),
                "load_avg": [round(float(v), 2) for v in load],
            }

    try:
        import psutil

        cpu_pct = psutil.cpu_percent(interval=0.1)
        mem = psutil.virtual_memory()
        disk = psutil.disk_usage("/")
        try:
            load = list(os.getloadavg())
        except (AttributeError, OSError):
            load = [0.0, 0.0, 0.0]
        return {
            "cpu_percent": round(float(cpu_pct), 1),
            "memory_used_mb": int(round(mem.used / (1024 * 1024))),
            "memory_total_mb": int(round(mem.total / (1024 * 1024))),
            "disk_used_gb": round(disk.used / (1024**3), 1),
            "disk_total_gb": round(disk.total / (1024**3), 1),
            "temp_c": _read_cpu_temp(),
            "load_avg": [round(float(v), 2) for v in load],
        }
    except ImportError:
        return {
            "cpu_percent": None,
            "memory_used_mb": None,
            "memory_total_mb": None,
            "disk_used_gb": None,
            "disk_total_gb": None,
            "temp_c": _read_cpu_temp(),
            "load_avg": [0.0, 0.0, 0.0],
        }


def _read_cpu_temp() -> float | None:
    """Best-effort SoC temperature read in degrees Celsius."""
    try:
        import psutil

        temps = psutil.sensors_temperatures() or {}
        # Walk the common sensor names. The first non-zero value wins.
        for key in (
            "soc_thermal",
            "cpu_thermal",
            "coretemp",
            "k10temp",
            "armada_thermal",
        ):
            entries = temps.get(key)
            if entries:
                for entry in entries:
                    val = getattr(entry, "current", None)
                    if isinstance(val, (int, float)) and val > 0:
                        return round(float(val), 1)
        # Fall through: any temp reading is better than nothing.
        for entries in temps.values():
            for entry in entries:
                val = getattr(entry, "current", None)
                if isinstance(val, (int, float)) and val > 0:
                    return round(float(val), 1)
    except Exception:
        pass
    # Final fallback: read a thermal-zone sysfs file directly.
    try:
        raw = Path("/sys/class/thermal/thermal_zone0/temp").read_text().strip()
        return round(int(raw) / 1000.0, 1)
    except (OSError, ValueError):
        return None


def _collect_network() -> dict[str, Any]:
    """Primary IPv4 + ethernet / wlan MAC."""
    return {
        "ip": _read_primary_ipv4(),
        "mac_eth0": _read_mac("eth0"),
        "mac_wlan0": _read_mac("wlan0"),
    }


def _read_mac(iface: str) -> str | None:
    try:
        mac = Path(f"/sys/class/net/{iface}/address").read_text().strip()
        return mac or None
    except OSError:
        return None


def _read_primary_ipv4() -> str | None:
    """Parse ``ip -4 addr show`` for the first non-loopback IPv4."""
    try:
        result = subprocess.run(
            ["ip", "-4", "-o", "addr", "show"],
            capture_output=True,
            text=True,
            timeout=1.0,
            check=False,
        )
    except (FileNotFoundError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    # Each line: "2: eth0    inet 192.168.1.42/24 brd ... scope global ..."
    pattern = re.compile(r"\binet\s+(\d+\.\d+\.\d+\.\d+)/")
    for line in result.stdout.splitlines():
        if " lo " in line or line.startswith("1: lo "):
            continue
        m = pattern.search(line)
        if m:
            return m.group(1)
    return None


def _collect_device() -> dict[str, Any]:
    """Device identity (device_id) from the runtime config."""
    device_id: str | None = None
    try:
        runtime = get_agent_app()
        cfg = getattr(runtime, "config", None)
        if cfg is not None:
            agent_cfg = getattr(cfg, "agent", None)
            if agent_cfg is not None:
                device_id = getattr(agent_cfg, "device_id", None)
    except Exception:
        device_id = None
    if not device_id:
        try:
            device_id = Path("/etc/ados/device_id").read_text().strip() or None
        except OSError:
            device_id = None
    return {"device_id": device_id or "--"}


async def _collect_logs(unit: str, *, count: int) -> list[str]:
    """Tail ``count`` lines from journalctl for ``unit``.

    The shell-out runs in a worker thread so the asyncio loop is not
    blocked. Stderr is captured too so a missing-unit error surfaces
    in the response rather than dropping the call to silence.
    """

    def _run() -> list[str]:
        try:
            result = subprocess.run(
                [
                    "journalctl",
                    "-u",
                    unit,
                    "-n",
                    str(int(count)),
                    "--no-pager",
                    "-o",
                    "cat",
                ],
                capture_output=True,
                text=True,
                timeout=2.0,
                check=False,
            )
        except (FileNotFoundError, subprocess.SubprocessError) as exc:
            return [f"<journalctl unavailable: {exc}>"]
        if result.returncode != 0:
            err = (result.stderr or "").strip().splitlines()
            tail = err[-1] if err else f"exit {result.returncode}"
            return [f"<journalctl error: {tail}>"]
        out = result.stdout.splitlines()
        if not out:
            return ["<no journal entries>"]
        return out[-count:]

    return await asyncio.to_thread(_run)
