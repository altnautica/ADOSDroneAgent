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
PAIRING_STATE_PATH = Path("/etc/ados/pairing.json")


def _load_api_key() -> str | None:
    """Read the local pairing.json and return the API key if paired.

    DEC-108 Phase E: when the agent is paired (api/middleware/auth.py
    enforces ApiKeyAuthMiddleware), the CLI must send the X-ADOS-Key
    header on every request to /api/* routes. The key lives at
    /etc/ados/pairing.json (written by install.sh during pairing).

    Returns the api_key string, or None if pairing.json is missing or
    unreadable. The CLI continues without the header in that case —
    auth-exempt endpoints (e.g. /api/pairing/info) still work.
    """
    try:
        import json
        if PAIRING_STATE_PATH.exists():
            with open(PAIRING_STATE_PATH) as f:
                data = json.load(f)
            return data.get("api_key")
    except (OSError, ValueError, json.JSONDecodeError):
        pass
    return None


def _auth_headers() -> dict[str, str]:
    """Build request headers including the agent API key when paired."""
    key = _load_api_key()
    return {"X-ADOS-Key": key} if key else {}


def _api_get(path: str, *, raw: bool = False) -> dict | None:
    """Make a GET request to the local agent REST API.

    Args:
        path: API endpoint path (e.g. "/api/status").
        raw: If True, return the raw JSON dict even on error status.

    Returns:
        Parsed JSON dict, or None on connection/HTTP error.
    """
    try:
        resp = httpx.get(f"{API_BASE}{path}", headers=_auth_headers(), timeout=5.0)
        resp.raise_for_status()
        return resp.json()
    except httpx.ConnectError:
        click.echo(
            "Error: Agent is not running. Start with 'ados start' or 'ados demo'.",
            err=True,
        )
        sys.exit(1)
    except (httpx.HTTPStatusError, httpx.HTTPError) as e:
        if isinstance(e, httpx.HTTPStatusError):
            click.echo(f"Error: API returned {e.response.status_code}", err=True)
        else:
            click.echo(f"Error: {e}", err=True)
        return None


@click.group(invoke_without_command=True)
@click.pass_context
def cli(ctx: click.Context) -> None:
    """ADOS Drone Agent CLI."""
    if ctx.invoked_subcommand is None:
        from ados.cli.help_display import show_help
        show_help()


# ─── INFO ───────────────────────────────────────────────────────────────────


@cli.command()
def version() -> None:
    """Print the agent version."""
    click.echo(f"ados-drone-agent v{__version__}")


@cli.command()
@click.option("--json", "as_json", is_flag=True, help="Output raw JSON for scripting.")
def status(as_json: bool) -> None:
    """Show agent status."""
    data = _api_get("/api/status")
    if data:
        if as_json:
            click.echo(json.dumps(data, indent=2))
            return
        click.echo(f"Version:    {data.get('version', '?')}")
        click.echo(f"Uptime:     {data.get('uptime_seconds', 0):.0f}s")
        click.echo(f"Board:      {data.get('board', {}).get('name', '?')}")
        click.echo(f"Tier:       {data.get('board', {}).get('tier', '?')}")
        click.echo(f"FC:         {data.get('fc_connected', False)}")
        click.echo(f"FC Port:    {data.get('fc_port', 'N/A')}")


@cli.command()
@click.option("--json", "as_json", is_flag=True, help="Output raw JSON for scripting.")
def health(as_json: bool) -> None:
    """Show system health (CPU, RAM, disk, temperature)."""
    data = _api_get("/api/status")
    if data:
        if as_json:
            click.echo(json.dumps(data.get("health", {}), indent=2))
            return
        h = data.get("health", {})
        click.echo(f"CPU:    {h.get('cpu_percent', 0):.1f}%")
        click.echo(f"Memory: {h.get('memory_percent', 0):.1f}%")
        click.echo(f"Disk:   {h.get('disk_percent', 0):.1f}%")
        temp = h.get("temperature")
        click.echo(f"Temp:   {temp:.1f}C" if temp else "Temp:   N/A")


@cli.command("help")
def help_cmd() -> None:
    """Show the rich CLI cheatsheet."""
    from ados.cli.help_display import show_help
    show_help()


# ─── CONFIG ─────────────────────────────────────────────────────────────────


@cli.command()
@click.argument("key", required=False, default=None)
def config(key: str | None) -> None:
    """Show config, or get a specific value by dot-path key.

    With no arguments, prints the full configuration.
    With a key (e.g. mavlink.baud), prints that specific value.
    """
    data = _api_get("/api/config")
    if not data:
        return
    if key is None:
        click.echo(json.dumps(data, indent=2))
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


@cli.command("set")
@click.argument("key")
@click.argument("value")
def set_config(key: str, value: str) -> None:
    """Set a config value (dot-separated path)."""
    try:
        resp = httpx.put(
            f"{API_BASE}/api/config",
            headers=_auth_headers(),
            json={"key": key, "value": value},
            timeout=5.0,
        )
        resp.raise_for_status()
        click.echo(f"Set {key} = {value}")
    except httpx.ConnectError:
        click.echo(
            "Error: Agent is not running. Start with 'ados start' or 'ados demo'.",
            err=True,
        )
        sys.exit(1)
    except httpx.HTTPError as e:
        if isinstance(e, httpx.HTTPStatusError):
            click.echo(f"Error: {e.response.status_code}", err=True)
        else:
            click.echo(f"Error: {e}", err=True)


# ─── FLIGHT ─────────────────────────────────────────────────────────────────


@cli.command()
def mavlink() -> None:
    """Show MAVLink/FC connection status."""
    data = _api_get("/api/status")
    if data:
        click.echo(f"Connected: {data.get('fc_connected', False)}")
        click.echo(f"Port:      {data.get('fc_port', 'N/A')}")
        click.echo(f"Baud:      {data.get('fc_baud', 'N/A')}")


@cli.command()
@click.option("--json", "as_json", is_flag=True, help="Output raw JSON for scripting.")
def video(as_json: bool) -> None:
    """Show video pipeline status."""
    data = _api_get("/api/video")
    if data:
        if as_json:
            click.echo(json.dumps(data, indent=2))
            return
        click.echo(f"State:     {data.get('state', '?')}")
        cameras = data.get("cameras", {}).get("cameras", [])
        click.echo(f"Cameras:   {len(cameras)}")
        for cam in cameras:
            click.echo(f"  - {cam.get('name', '?')} ({cam.get('type', '?')})")
        recorder = data.get("recorder", {})
        click.echo(f"Recording: {recorder.get('recording', False)}")
        mtx = data.get("mediamtx", {})
        click.echo(f"MediaMTX:  {'running' if mtx.get('running') else 'stopped'}")


@cli.command()
def snap() -> None:
    """Capture a JPEG snapshot from the video pipeline."""
    try:
        resp = httpx.post(f"{API_BASE}/api/video/snapshot", headers=_auth_headers(), timeout=10.0)
        resp.raise_for_status()
        data = resp.json()
        if data.get("error"):
            click.echo(f"Error: {data['error']}", err=True)
        else:
            click.echo(f"Snapshot saved: {data.get('path', '?')}")
    except httpx.ConnectError:
        click.echo(
            "Error: Agent is not running. Start with 'ados start' or 'ados demo'.",
            err=True,
        )
        sys.exit(1)
    except httpx.HTTPError as e:
        click.echo(f"Error: {e}", err=True)


@cli.command()
@click.option("--json", "as_json", is_flag=True, help="Output raw JSON for scripting.")
def link(as_json: bool) -> None:
    """Show WFB-ng video link status."""
    data = _api_get("/api/wfb")
    if data:
        if as_json:
            click.echo(json.dumps(data, indent=2))
            return
        click.echo(f"State:    {data.get('state', '?')}")
        click.echo(f"RSSI:     {data.get('rssi_dbm', -100)} dBm")
        click.echo(f"SNR:      {data.get('snr_db', 0):.1f} dB")
        click.echo(f"Channel:  {data.get('channel', '?')}")
        rx = data.get('packets_received', 0)
        lost = data.get('packets_lost', 0)
        click.echo(f"Packets:  {rx} rx, {lost} lost")
        fec_r = data.get('fec_recovered', 0)
        fec_f = data.get('fec_failed', 0)
        click.echo(f"FEC:      {fec_r} recovered, {fec_f} failed")
        click.echo(f"Bitrate:  {data.get('bitrate_kbps', 0)} kbps")


# ─── SCRIPTING ──────────────────────────────────────────────────────────────


@cli.command()
@click.option("--json", "as_json", is_flag=True, help="Output raw JSON for scripting.")
def scripts(as_json: bool) -> None:
    """List running scripts."""
    data = _api_get("/api/scripts")
    if data:
        if as_json:
            click.echo(json.dumps(data, indent=2))
            return
        script_list = data.get("scripts", [])
        if not script_list:
            click.echo("No scripts running.")
            return
        for s in script_list:
            sid = s.get('script_id', '?')
            fname = s.get('filename', '?')
            click.echo(f"  [{s.get('state', '?')}] {sid}: {fname}")


@cli.command()
@click.argument("path")
def run(path: str) -> None:
    """Run a Python script on the agent."""
    try:
        resp = httpx.post(f"{API_BASE}/api/scripts/run", headers=_auth_headers(), json={"path": path}, timeout=5.0)
        resp.raise_for_status()
        data = resp.json()
        click.echo(f"Started: {data.get('script_id', '?')}")
    except httpx.ConnectError:
        click.echo(
            "Error: Agent is not running. Start with 'ados start' or 'ados demo'.",
            err=True,
        )
        sys.exit(1)
    except httpx.HTTPError as e:
        if isinstance(e, httpx.HTTPStatusError):
            click.echo(f"Error: {e.response.status_code} - {e.response.text}", err=True)
        else:
            click.echo(f"Error: {e}", err=True)


@cli.command()
@click.argument("command")
def send(command: str) -> None:
    """Send a text command to the scripting engine."""
    try:
        resp = httpx.post(
            f"{API_BASE}/api/scripting/command",
            headers=_auth_headers(),
            json={"command": command},
            timeout=5.0,
        )
        resp.raise_for_status()
        data = resp.json()
        click.echo(data.get("result", "ok"))
    except httpx.ConnectError:
        click.echo(
            "Error: Agent is not running. Start with 'ados start' or 'ados demo'.",
            err=True,
        )
        sys.exit(1)
    except httpx.HTTPError as e:
        if isinstance(e, httpx.HTTPStatusError):
            click.echo(f"Error: {e.response.status_code}", err=True)
        else:
            click.echo(f"Error: {e}", err=True)


# ─── OTA ────────────────────────────────────────────────────────────────────


@cli.command()
@click.option("--json", "as_json", is_flag=True, help="Output raw JSON for scripting.")
def update(as_json: bool) -> None:
    """Show OTA update status."""
    data = _api_get("/api/ota")
    if data:
        if as_json:
            click.echo(json.dumps(data, indent=2))
            return
        click.echo(f"Version:    {data.get('current_version', '?')}")
        click.echo(f"State:      {data.get('state', '?')}")
        click.echo(f"Channel:    {data.get('channel', '?')}")
        click.echo(f"Repo:       {data.get('github_repo', '?')}")
        last_check = data.get("last_check", "")
        click.echo(f"Last check: {last_check or 'never'}")
        prev = data.get("previous_version", "")
        if prev:
            click.echo(f"Previous:   {prev}")
        pending = data.get("pending_update")
        if pending:
            click.echo(f"Update:     v{pending.get('version', '?')} available")


@cli.command()
def check() -> None:
    """Check for available OTA updates."""
    try:
        resp = httpx.post(f"{API_BASE}/api/ota/check", headers=_auth_headers(), timeout=30.0)
        resp.raise_for_status()
        data = resp.json()
        if data.get("status") == "update_available":
            click.echo(f"Update available: v{data.get('version', '?')}")
            changelog = data.get("changelog", "")
            if changelog:
                click.echo(f"Changelog: {changelog[:200]}")
        else:
            click.echo("No updates available.")
    except httpx.ConnectError:
        click.echo(
            "Error: Agent is not running. Start with 'ados start' or 'ados demo'.",
            err=True,
        )
        sys.exit(1)
    except httpx.HTTPError as e:
        if isinstance(e, httpx.HTTPStatusError):
            click.echo(f"Error: {e.response.status_code}", err=True)
        else:
            click.echo(f"Error: {e}", err=True)


@cli.command()
@click.option("--yes", "-y", is_flag=True, default=False, help="Skip confirmation prompt")
def upgrade(yes: bool) -> None:
    """One-step check, download, install, and restart."""
    # Check
    click.echo("Checking for updates...")
    try:
        resp = httpx.post(f"{API_BASE}/api/ota/check", headers=_auth_headers(), timeout=30.0)
        resp.raise_for_status()
        data = resp.json()
    except httpx.ConnectError:
        click.echo(
            "Error: Agent is not running. Start with 'ados start' or 'ados demo'.",
            err=True,
        )
        sys.exit(1)
    except httpx.HTTPError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)

    if data.get("status") != "update_available":
        click.echo("Already up to date.")
        return

    new_version = data.get("version", "?")
    click.echo(f"Update available: v{new_version}")

    if not yes:
        click.confirm(f"Install v{new_version}?", abort=True)

    # Install
    click.echo("Downloading and installing...")
    try:
        resp = httpx.post(f"{API_BASE}/api/ota/install", headers=_auth_headers(), timeout=300.0)
        resp.raise_for_status()
        result = resp.json()
    except httpx.HTTPError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)

    if result.get("status") == "error":
        click.echo(f"Failed: {result.get('message', '?')}", err=True)
        sys.exit(1)

    click.echo(f"Installed v{new_version}")

    # Restart
    click.echo("Restarting service...")
    try:
        resp = httpx.post(f"{API_BASE}/api/ota/restart", headers=_auth_headers(), timeout=30.0)
        resp.raise_for_status()
        result = resp.json()
        click.echo(result.get("message", "Done."))
    except httpx.HTTPError:
        click.echo("Restart request sent. Service may already be restarting.")


@cli.command("rollback")
@click.argument("version", required=False, default=None)
def rollback_cmd(version: str | None) -> None:
    """Rollback to a previous version (from PyPI)."""
    try:
        params = {}
        if version:
            params["version"] = version
        resp = httpx.post(f"{API_BASE}/api/ota/rollback", headers=_auth_headers(), params=params, timeout=120.0)
        resp.raise_for_status()
        data = resp.json()
        if data.get("status") == "rolled_back":
            click.echo(data.get("message", "Rollback complete."))
        else:
            click.echo(f"Failed: {data.get('message', '?')}", err=True)
            sys.exit(1)
    except httpx.ConnectError:
        click.echo(
            "Error: Agent is not running. Start with 'ados start' or 'ados demo'.",
            err=True,
        )
        sys.exit(1)
    except httpx.HTTPError as e:
        if isinstance(e, httpx.HTTPStatusError):
            click.echo(f"Error: {e.response.status_code}", err=True)
        else:
            click.echo(f"Error: {e}", err=True)


# ─── PAIRING ───────────────────────────────────────────────────────────────


@cli.command()
@click.option("--json", "as_json", is_flag=True, help="Output raw JSON for scripting.")
def pair(as_json: bool) -> None:
    """Show pairing status and code.

    Displays the pairing code box when unpaired, or owner info when paired.
    """
    data = _api_get("/api/pairing/info")
    if data:
        if as_json:
            click.echo(json.dumps(data, indent=2))
            return
        click.echo(f"Paired:     {data.get('paired', False)}")
        click.echo(f"Device ID:  {data.get('device_id', '?')}")
        click.echo(f"Name:       {data.get('name', '?')}")
        click.echo(f"Board:      {data.get('board', '?')}")
        click.echo(f"mDNS:       {data.get('mdns_host', '?')}")
        if data.get("paired"):
            click.echo(f"Owner:      {data.get('owner_id', '?')}")
            paired_at = data.get("paired_at")
            if paired_at:
                import datetime
                ts = datetime.datetime.fromtimestamp(paired_at).isoformat()
                click.echo(f"Paired at:  {ts}")
        else:
            code = data.get("pairing_code", "?")
            click.echo("")
            click.echo("  +--------+")
            click.echo(f"  | {code} |")
            click.echo("  +--------+")
            click.echo("")
            click.echo("Enter this code in ADOS Mission Control to pair.")


@cli.command()
@click.confirmation_option(prompt="This will unpair the agent. Continue?")
def unpair() -> None:
    """Unpair the agent and generate a new pairing code."""
    try:
        resp = httpx.post(f"{API_BASE}/api/pairing/unpair", headers=_auth_headers(), timeout=5.0)
        if resp.status_code == 409:
            click.echo("Agent is not currently paired.")
            return
        resp.raise_for_status()
        data = resp.json()
        new_code = data.get("new_code", "?")
        click.echo("Agent unpaired successfully.")
        click.echo(f"New pairing code: {new_code}")
    except httpx.ConnectError:
        click.echo(
            "Error: Agent is not running. Start with 'ados start' or 'ados demo'.",
            err=True,
        )
        sys.exit(1)
    except httpx.HTTPError as e:
        if isinstance(e, httpx.HTTPStatusError):
            click.echo(f"Error: {e.response.status_code}", err=True)
        else:
            click.echo(f"Error: {e}", err=True)


# ─── TOOLS ──────────────────────────────────────────────────────────────────


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


@cli.command()
@click.option(
    "--keep-config", is_flag=True, default=True,
    help="Keep /etc/ados config (default: yes)",
)
@click.option("--purge", is_flag=True, default=False, help="Remove everything including config")
@click.option("--yes", "-y", is_flag=True, default=False, help="Skip confirmation prompt")
def uninstall(keep_config: bool, purge: bool, yes: bool) -> None:
    """Uninstall the ADOS Drone Agent from this system."""
    is_linux = platform.system() == "Linux"
    is_mac = platform.system() == "Darwin"

    if not is_linux and not is_mac:
        click.echo(f"Unsupported platform: {platform.system()}", err=True)
        sys.exit(1)

    # macOS: pip/pipx/uv uninstall
    if is_mac:
        if not yes:
            click.confirm("Uninstall ados-drone-agent from this system?", abort=True)

        pkg = "ados-drone-agent"
        # Detect installer: pipx > uv > pip
        installer = None
        try:
            result = subprocess.run(
                ["pipx", "list", "--short"],
                capture_output=True, text=True, timeout=10,
            )
            if result.returncode == 0 and pkg in result.stdout:
                installer = "pipx"
        except FileNotFoundError:
            pass

        if installer is None:
            try:
                result = subprocess.run(
                    ["uv", "tool", "list"],
                    capture_output=True, text=True, timeout=10,
                )
                if result.returncode == 0 and pkg in result.stdout:
                    installer = "uv"
            except FileNotFoundError:
                pass

        if installer is None:
            installer = "pip"

        click.echo(f"Detected installer: {installer}")

        if installer == "pipx":
            cmd = ["pipx", "uninstall", pkg]
        elif installer == "uv":
            cmd = ["uv", "tool", "uninstall", pkg]
        else:
            cmd = [sys.executable, "-m", "pip", "uninstall", "-y", pkg]

        click.echo(f"Running: {' '.join(cmd)}")
        result = subprocess.run(cmd, capture_output=True, text=True)
        if result.returncode == 0:
            click.echo("Uninstalled successfully.")
        else:
            click.echo(f"Uninstall failed: {result.stderr.strip()}", err=True)
            sys.exit(1)
        return

    # Linux: full system uninstall
    install_dir = Path("/opt/ados")
    config_dir = Path("/etc/ados")
    data_dir = Path("/var/ados")
    service_name = "ados-agent"
    service_file = Path("/etc/systemd/system/ados-agent.service")
    symlinks = [Path("/usr/local/bin/ados"), Path("/usr/local/bin/ados-agent")]

    if os.geteuid() != 0:
        click.echo("Error: Uninstall requires root. Run with sudo.", err=True)
        sys.exit(1)

    # Show what will be removed
    items = []
    if service_file.exists():
        items.append(f"  systemd service: {service_name}")
    for sym in symlinks:
        if sym.exists() or sym.is_symlink():
            items.append(f"  symlink: {sym}")
    if install_dir.exists():
        items.append(f"  install dir: {install_dir}")
    if data_dir.exists():
        items.append(f"  data dir: {data_dir}")
    if purge and config_dir.exists():
        items.append(f"  config dir: {config_dir}")

    if not items:
        click.echo("Nothing to uninstall. ADOS Drone Agent is not installed.")
        return

    click.echo("The following will be removed:")
    for item in items:
        click.echo(item)
    if not purge and config_dir.exists():
        click.echo(f"  (keeping config: {config_dir})")
    click.echo("")

    if not yes:
        click.confirm("Proceed with uninstall?", abort=True)

    # 1. Stop and disable systemd service
    if service_file.exists():
        click.echo(f"Stopping {service_name}...")
        subprocess.run(
            ["systemctl", "stop", service_name],
            capture_output=True, timeout=30,
        )
        subprocess.run(
            ["systemctl", "disable", service_name],
            capture_output=True, timeout=10,
        )
        try:
            service_file.unlink()
            click.echo(f"Removed {service_file}")
        except OSError:
            pass
        subprocess.run(
            ["systemctl", "daemon-reload"],
            capture_output=True, timeout=10,
        )

    # 2. Remove symlinks
    for sym in symlinks:
        try:
            if sym.exists() or sym.is_symlink():
                sym.unlink()
                click.echo(f"Removed {sym}")
        except OSError:
            pass

    # 3. Remove install dir
    if install_dir.exists():
        shutil.rmtree(install_dir, ignore_errors=True)
        click.echo(f"Removed {install_dir}")

    # 4. Remove data dir
    if data_dir.exists():
        shutil.rmtree(data_dir, ignore_errors=True)
        click.echo(f"Removed {data_dir}")

    # 5. Config dir
    if purge and config_dir.exists():
        shutil.rmtree(config_dir, ignore_errors=True)
        click.echo(f"Removed {config_dir}")
    elif config_dir.exists():
        click.echo(f"Config preserved at {config_dir}. Use --purge to remove.")

    click.echo("")
    click.echo("ADOS Drone Agent uninstalled successfully.")


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


# ─── SYSTEM ─────────────────────────────────────────────────────────────────


@cli.command()
@click.option("--lines", "-n", default=50, help="Number of log lines to show.")
@click.option("--follow", "-f", is_flag=True, help="Follow log output (tail -f).")
@click.option("--since", "-s", default=None, help="Show logs since (e.g. '1h ago', '2024-01-01').")
def logs(lines: int, follow: bool, since: str | None) -> None:
    """Show agent logs (journalctl wrapper on Linux, file tail on macOS)."""
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
