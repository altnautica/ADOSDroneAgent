"""System info collection helpers and the full diagnostics renderer.

Used by `ados diag`. Pulled out of cli/main.py to keep the CLI module
focused on click command wiring instead of formatting and probe logic.
"""

from __future__ import annotations

import os
import platform
import shutil
import socket
import subprocess
import time
from importlib.metadata import version as pkg_version
from pathlib import Path

import click
import httpx
import psutil

from ados import __version__
from ados.core.paths import CONFIG_YAML, DEVICE_ID_PATH

API_BASE = "http://localhost:8080"


def _auth_headers() -> dict[str, str]:
    """Build auth headers for local agent API calls.

    Local-import to avoid circular dependency with cli.main and to keep
    this module free of click decorators.
    """
    from .main import _auth_headers as _ah

    return _ah()


def _section(title: str) -> str:
    """Return a formatted section header."""
    return f"\n{'=' * 40}\n  {title}\n{'=' * 40}"


def _safe_read(filepath: str) -> str:
    """Read a file, returning empty string on failure."""
    try:
        return Path(filepath).read_text().strip().rstrip("\x00")
    except (OSError, PermissionError):
        return ""


def _get_cpu_temp() -> str:
    """Get CPU temperature, cross-platform."""
    try:
        temps = psutil.sensors_temperatures()
        if temps:
            for key in ("cpu_thermal", "cpu-thermal", "coretemp", "k10temp"):
                if key in temps and temps[key]:
                    return f"{temps[key][0].current:.1f} C"
    except (AttributeError, OSError):
        pass
    return "N/A"


def _get_uptime() -> str:
    """Get system uptime as human-readable string."""
    boot = psutil.boot_time()
    uptime_secs = int(time.time() - boot)
    days, remainder = divmod(uptime_secs, 86400)
    hours, remainder = divmod(remainder, 3600)
    minutes, _ = divmod(remainder, 60)
    parts = []
    if days > 0:
        parts.append(f"{days}d")
    if hours > 0:
        parts.append(f"{hours}h")
    parts.append(f"{minutes}m")
    return " ".join(parts)


def _get_ip_addresses() -> list[str]:
    """Get non-loopback IP addresses."""
    addrs = []
    try:
        for iface_name, iface_addrs in psutil.net_if_addrs().items():
            for addr in iface_addrs:
                if addr.family == socket.AF_INET and not addr.address.startswith("127."):
                    addrs.append(f"{iface_name}: {addr.address}")
    except (OSError, AttributeError):
        pass
    return addrs if addrs else ["none detected"]


def _get_service_status(service: str) -> str:
    """Get systemd service status. Returns 'N/A (no systemd)' on macOS."""
    if platform.system() == "Darwin":
        return "N/A (no systemd)"
    try:
        result = subprocess.run(
            ["systemctl", "is-active", service],
            capture_output=True, text=True, timeout=5,
        )
        return result.stdout.strip() or "unknown"
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return "unknown"


def _disk_usage_line(path: str) -> str:
    """Format disk usage for a path."""
    try:
        usage = shutil.disk_usage(path)
        total_gb = usage.total / (1024 ** 3)
        used_gb = usage.used / (1024 ** 3)
        free_gb = usage.free / (1024 ** 3)
        pct = (usage.used / usage.total) * 100
        return f"{used_gb:.1f}G / {total_gb:.1f}G ({pct:.0f}% used, {free_gb:.1f}G free)"
    except OSError:
        return "unavailable"


def _safe_pkg_version(pkg_name: str) -> str:
    """Get installed version of a Python package."""
    try:
        return pkg_version(pkg_name)
    except Exception:
        return "not installed"


def _get_fc_info() -> dict:
    """Try to get FC info from the running agent API."""
    try:
        resp = httpx.get(f"{API_BASE}/api/status", headers=_auth_headers(), timeout=3.0)
        resp.raise_for_status()
        data = resp.json()
        return {
            "connected": data.get("fc_connected", False),
            "port": data.get("fc_port", "N/A"),
            "baud": data.get("fc_baud", "N/A"),
            "firmware": data.get("fc_firmware", "unknown"),
        }
    except Exception:
        return {
            "connected": False,
            "port": "N/A",
            "baud": "N/A",
            "firmware": "agent not running",
        }


def render_diag() -> None:
    """Print the full system diagnostics dump."""
    from ados.hal.detect import detect_board

    board = detect_board()

    # Board
    click.echo(_section("Board"))
    click.echo(f"  Name:          {board.name}")
    click.echo(f"  Model:         {board.model}")
    click.echo(f"  Tier:          {board.tier}")
    click.echo(f"  RAM (total):   {board.ram_mb} MB")
    click.echo(f"  CPU cores:     {board.cpu_cores}")
    click.echo(f"  Architecture:  {platform.machine()}")

    # System
    click.echo(_section("System"))
    click.echo(f"  OS:            {platform.system()} {platform.release()}")
    if Path("/etc/os-release").exists():
        os_pretty = _safe_read("/etc/os-release")
        for line in os_pretty.splitlines():
            if line.startswith("PRETTY_NAME="):
                distro_name = line.split("=", 1)[1].strip('"')
                click.echo(f"  Distro:        {distro_name}")
                break
    click.echo(f"  Kernel:        {platform.release()}")
    click.echo(f"  Python:        {platform.python_version()}")
    click.echo(f"  Uptime:        {_get_uptime()}")

    # Network
    click.echo(_section("Network"))
    click.echo(f"  Hostname:      {socket.gethostname()}")
    for addr_line in _get_ip_addresses():
        click.echo(f"  IP:            {addr_line}")

    # Services
    click.echo(_section("Services"))
    for svc in ["ados-supervisor", "ados-api", "ados-mavlink", "ados-cloud"]:
        st = _get_service_status(svc)
        click.echo(f"  {svc:20s} {st}")

    # FC Connection
    fc = _get_fc_info()
    click.echo(_section("Flight Controller"))
    click.echo(f"  Connected:     {fc['connected']}")
    click.echo(f"  Port:          {fc['port']}")
    click.echo(f"  Baud:          {fc['baud']}")
    click.echo(f"  Firmware:      {fc['firmware']}")

    # Disk
    click.echo(_section("Disk"))
    click.echo(f"  /              {_disk_usage_line('/')}")
    for dpath in ["/etc/ados", "/var/log"]:
        if Path(dpath).exists():
            click.echo(f"  {dpath:14s} {_disk_usage_line(dpath)}")

    # RAM
    click.echo(_section("Memory"))
    mem = psutil.virtual_memory()
    click.echo(f"  Total:         {mem.total // (1024 ** 2)} MB")
    click.echo(f"  Used:          {mem.used // (1024 ** 2)} MB")
    click.echo(f"  Free:          {mem.available // (1024 ** 2)} MB")
    click.echo(f"  Percent:       {mem.percent}%")

    # CPU
    click.echo(_section("CPU"))
    click.echo(f"  Cores:         {psutil.cpu_count(logical=True)}")
    freq = psutil.cpu_freq()
    if freq:
        click.echo(f"  Frequency:     {freq.current:.0f} MHz")
    load = os.getloadavg()
    click.echo(f"  Load avg:      {load[0]:.2f}  {load[1]:.2f}  {load[2]:.2f}")

    # Temperature
    click.echo(_section("Temperature"))
    click.echo(f"  CPU:           {_get_cpu_temp()}")

    # Agent
    click.echo(_section("Agent"))
    click.echo(f"  Version:       {__version__}")
    config_path = os.environ.get("ADOS_CONFIG", str(CONFIG_YAML))
    click.echo(f"  Config:        {config_path}")
    device_id_path = str(DEVICE_ID_PATH)
    device_id = _safe_read(device_id_path) or os.environ.get("ADOS_DEVICE_ID", "not set")
    click.echo(f"  Device ID:     {device_id}")

    try:
        from ados.core.config import load_config
        cfg = load_config()
        click.echo(f"  Log level:     {cfg.logging.level}")
    except Exception:
        click.echo("  Log level:     unknown")

    # Dependencies
    click.echo(_section("Dependencies"))
    deps = [
        "pymavlink", "fastapi", "uvicorn", "paho-mqtt", "pyyaml",
        "pydantic", "click", "websockets", "structlog", "pyserial",
        "textual", "psutil", "httpx", "cryptography",
    ]
    for dep in deps:
        click.echo(f"  {dep:20s} {_safe_pkg_version(dep)}")

    click.echo("")
