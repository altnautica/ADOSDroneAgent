"""CLI interface for ADOS Drone Agent."""

from __future__ import annotations

import json
import sys

import click
import httpx

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
        click.echo("Error: TUI dependencies not installed. Install with: pip install textual", err=True)
        sys.exit(1)


@cli.command()
def start() -> None:
    """Start the ADOS Drone Agent."""
    from ados.core.main import main as agent_main
    agent_main()


if __name__ == "__main__":
    cli()
