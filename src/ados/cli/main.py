"""CLI interface for ADOS Drone Agent."""

from __future__ import annotations

import json
import os
import platform
import shutil
import signal
import socket
import subprocess
import sys
import time
from importlib.metadata import version as pkg_version
from pathlib import Path

import click
import httpx
import psutil

from ados import __version__

API_BASE = "http://localhost:8080"


def _api_get(path: str) -> dict | None:
    """Make a GET request to the local agent REST API."""
    try:
        resp = httpx.get(f"{API_BASE}{path}", timeout=5.0)
        resp.raise_for_status()
        return resp.json()
    except httpx.ConnectError:
        click.echo("Error: Agent is not running. Start it with 'ados start'.", err=True)
        return None
    except httpx.HTTPStatusError as e:
        click.echo(f"Error: API returned {e.response.status_code}", err=True)
        return None


@click.group()
def cli() -> None:
    """ADOS Drone Agent CLI."""
    pass


@cli.command()
def version() -> None:
    """Print the agent version."""
    click.echo(f"ados-drone-agent v{__version__}")


@cli.command()
def status() -> None:
    """Show agent status."""
    data = _api_get("/api/status")
    if data:
        click.echo(f"Version:    {data.get('version', '?')}")
        click.echo(f"Uptime:     {data.get('uptime_seconds', 0):.0f}s")
        click.echo(f"Board:      {data.get('board', {}).get('name', '?')}")
        click.echo(f"Tier:       {data.get('board', {}).get('tier', '?')}")
        click.echo(f"FC:         {data.get('fc_connected', False)}")
        click.echo(f"FC Port:    {data.get('fc_port', 'N/A')}")


@cli.command()
def health() -> None:
    """Show system health."""
    data = _api_get("/api/status")
    if data:
        h = data.get("health", {})
        click.echo(f"CPU:    {h.get('cpu_percent', 0):.1f}%")
        click.echo(f"Memory: {h.get('memory_percent', 0):.1f}%")
        click.echo(f"Disk:   {h.get('disk_percent', 0):.1f}%")
        temp = h.get("temperature")
        click.echo(f"Temp:   {temp:.1f}C" if temp else "Temp:   N/A")


@cli.group()
def config() -> None:
    """Configuration commands."""
    pass


@config.command("show")
def config_show() -> None:
    """Print current configuration."""
    data = _api_get("/api/config")
    if data:
        click.echo(json.dumps(data, indent=2))


@config.command("get")
@click.argument("key")
def config_get(key: str) -> None:
    """Get a specific config value (dot-separated path)."""
    data = _api_get("/api/config")
    if not data:
        return
    parts = key.split(".")
    val = data
    for part in parts:
        if isinstance(val, dict) and part in val:
            val = val[part]
        else:
            click.echo(f"Key not found: {key}", err=True)
            return
    if isinstance(val, dict):
        click.echo(json.dumps(val, indent=2))
    else:
        click.echo(val)


@config.command("set")
@click.argument("key")
@click.argument("value")
def config_set(key: str, value: str) -> None:
    """Set a config value (dot-separated path)."""
    try:
        resp = httpx.put(
            f"{API_BASE}/api/config",
            json={"key": key, "value": value},
            timeout=5.0,
        )
        resp.raise_for_status()
        click.echo(f"Set {key} = {value}")
    except httpx.ConnectError:
        click.echo("Error: Agent is not running.", err=True)
    except httpx.HTTPStatusError as e:
        click.echo(f"Error: {e.response.status_code}", err=True)


@cli.group()
def mavlink() -> None:
    """MAVLink commands."""
    pass


@mavlink.command("status")
def mavlink_status() -> None:
    """Show MAVLink connection status."""
    data = _api_get("/api/status")
    if data:
        click.echo(f"Connected: {data.get('fc_connected', False)}")
        click.echo(f"Port:      {data.get('fc_port', 'N/A')}")
        click.echo(f"Baud:      {data.get('fc_baud', 'N/A')}")


@cli.command()
def tui() -> None:
    """Launch the TUI dashboard."""
    try:
        from ados.tui.app import ADOSTui
        app = ADOSTui()
        app.run()
    except ImportError:
        click.echo(
            "Error: TUI deps not installed. Install with: pip install textual",
            err=True,
        )
        sys.exit(1)


@cli.command()
def start() -> None:
    """Start the ADOS Drone Agent."""
    from ados.core.main import main as agent_main
    agent_main()


@cli.command()
@click.option("--port", default=8080, help="REST API port")
def demo(port: int) -> None:
    """Start in demo mode with simulated telemetry."""
    import asyncio

    from ados.core.config import load_config
    from ados.core.logging import configure_logging
    from ados.core.main import AgentApp

    config = load_config()
    config.server.mode = "disabled"
    config.scripting.rest_api.port = port
    configure_logging(
        level=config.logging.level,
        drone_name=config.agent.name,
        device_id=config.agent.device_id,
    )

    app = AgentApp(config, demo=True)

    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)

    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, app.request_shutdown)

    try:
        loop.run_until_complete(app.start())
    except KeyboardInterrupt:
        app.request_shutdown()
    finally:
        loop.close()


# ─── Diagnostics helpers ────────────────────────────────────────────────────


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
        resp = httpx.get(f"{API_BASE}/api/status", timeout=3.0)
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


@cli.command()
def diag() -> None:
    """Full system diagnostics dump."""
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
    for svc in ["ados-agent", "wfb_rx", "wfb_tx", "mavlink-router"]:
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
    config_path = os.environ.get("ADOS_CONFIG", "/etc/ados/config.yaml")
    click.echo(f"  Config:        {config_path}")
    device_id_path = "/etc/ados/device-id"
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


@cli.command()
@click.option("--lines", "-n", default=50, help="Number of log lines to show.")
@click.option("--follow", "-f", is_flag=True, help="Follow log output (tail -f).")
@click.option("--since", "-s", default=None, help="Show logs since (e.g. '1h ago', '2024-01-01').")
def logs(lines: int, follow: bool, since: str | None) -> None:
    """Show agent logs (journalctl wrapper)."""
    if platform.system() == "Darwin":
        # macOS has no systemd. Try reading from log files instead.
        log_paths = [
            Path.home() / ".ados" / "agent.log",
            Path("/tmp/ados-agent.log"),
        ]
        found = None
        for lp in log_paths:
            if lp.exists():
                found = lp
                break

        if found is None:
            click.echo("systemd is not available on macOS.")
            click.echo("No log file found at ~/.ados/agent.log or /tmp/ados-agent.log")
            click.echo("Start the agent with 'ados start' or 'ados demo' to generate logs.")
            return

        click.echo(f"Reading logs from {found}")
        click.echo("")
        try:
            if follow:
                subprocess.run(["tail", "-f", "-n", str(lines), str(found)])
            else:
                with open(found) as f:
                    all_lines = f.readlines()
                    for line in all_lines[-lines:]:
                        click.echo(line, nl=False)
        except KeyboardInterrupt:
            pass
        return

    # Linux: use journalctl
    cmd = ["journalctl", "-u", "ados-agent.service", "--no-pager"]

    if follow:
        cmd.append("-f")

    cmd.extend(["-n", str(lines)])

    if since:
        cmd.extend(["--since", since])

    try:
        subprocess.run(cmd)
    except FileNotFoundError:
        click.echo("journalctl not found. Is systemd installed?", err=True)
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    cli()
