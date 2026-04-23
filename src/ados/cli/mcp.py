"""`ados mcp ...` subcommands for the MCP server.

Provides pairing, token management, audit tailing, and a stdio bridge
for external MCP clients (Claude Desktop, Cursor, VS Code, custom scripts).

Registered under the main `cli` group in `ados.cli.main` via
`cli.add_command(mcp_group)`.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import click
import httpx

MCP_API_BASE = "http://127.0.0.1:8090/mcp-api"
PAIRING_STATE_PATH = Path("/etc/ados/pairing.json")


def _agent_key() -> dict[str, str]:
    """Return auth headers using the local agent pairing key."""
    try:
        if PAIRING_STATE_PATH.exists():
            data = json.loads(PAIRING_STATE_PATH.read_text())
            key = data.get("api_key", "")
            if key:
                return {"X-ADOS-Key": key}
    except Exception:
        pass
    return {}


def _get(path: str) -> dict | list | None:
    try:
        resp = httpx.get(f"{MCP_API_BASE}{path}", headers=_agent_key(), timeout=5.0)
        resp.raise_for_status()
        return resp.json()
    except httpx.HTTPError as e:
        click.echo(f"Error: {e}", err=True)
        return None


def _post(path: str, body: dict | None = None) -> dict | None:
    try:
        resp = httpx.post(
            f"{MCP_API_BASE}{path}",
            json=body or {},
            headers=_agent_key(),
            timeout=5.0,
        )
        resp.raise_for_status()
        return resp.json()
    except httpx.HTTPError as e:
        click.echo(f"Error: {e}", err=True)
        return None


# ---------------------------------------------------------------------------
# Group
# ---------------------------------------------------------------------------

@click.group("mcp")
def mcp_group() -> None:
    """Manage the on-drone MCP server: pairing, tokens, audit, stdio bridge."""


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------

@mcp_group.command("pair")
@click.option("--client-hint", default="ados-cli", help="Label for this client session.")
def mcp_pair(client_hint: str) -> None:
    """Mint a new session token and display the 6-word pairing mnemonic.

    Copy the mnemonic into your MCP client config as the bearer token.
    """
    result = _post("/pair", {"client_hint": client_hint})
    if not result:
        sys.exit(1)

    click.echo("")
    click.echo("  Pairing mnemonic (copy into your MCP client):")
    click.echo("")
    click.secho(f"  {result['mnemonic']}", fg="green", bold=True)
    click.echo("")
    click.echo(f"  Token ID : {result['token_id']}")
    click.echo(f"  Scopes   : {', '.join(result.get('scopes', []))}")

    import datetime
    exp = result.get("expires_at")
    if exp:
        dt = datetime.datetime.fromtimestamp(float(exp)).strftime("%Y-%m-%d %H:%M")
        click.echo(f"  Expires  : {dt}")
    click.echo("")


@mcp_group.command("status")
def mcp_status() -> None:
    """Show MCP service health, active token count, and operator-present state."""
    result = _get("/status")
    if not result:
        sys.exit(1)
    click.echo(f"Status            : {result.get('status', 'unknown')}")
    click.echo(f"Version           : {result.get('version', '?')}")
    click.echo(f"Active tokens     : {result.get('active_tokens', 0)}")
    click.echo(f"Operator present  : {result.get('operator_present', False)}")


@mcp_group.command("tokens")
@click.argument("action", type=click.Choice(["list", "revoke"]))
@click.argument("token_id", required=False, default=None)
@click.option("--json", "as_json", is_flag=True, help="Output raw JSON.")
def mcp_tokens(action: str, token_id: str | None, as_json: bool) -> None:
    """List or revoke session tokens.

    Examples:
      ados mcp tokens list
      ados mcp tokens revoke abc123de
    """
    if action == "list":
        result = _get("/tokens")
        if result is None:
            sys.exit(1)
        if as_json:
            click.echo(json.dumps(result, indent=2))
            return
        if not result:
            click.echo("No tokens found.")
            return
        for t in result:
            status_badge = "ACTIVE" if t.get("active") else ("REVOKED" if t.get("revoked") else "EXPIRED")
            click.echo(
                f"  [{status_badge}] {t['token_id']}  {t['client_hint']:30s}  scopes={','.join(t.get('scopes', []))}"
            )
    elif action == "revoke":
        if not token_id:
            click.echo("Error: token_id required for revoke.", err=True)
            sys.exit(1)
        result = _post(f"/tokens/{token_id}/revoke")
        if result:
            click.echo(f"Token {token_id} revoked.")


@mcp_group.command("audit")
@click.option("--n", default=50, help="Number of recent entries to show.")
@click.option("--json", "as_json", is_flag=True, help="Output raw JSON.")
def mcp_audit(n: int, as_json: bool) -> None:
    """Tail the MCP audit log (most recent N entries)."""
    result = _get(f"/audit/tail?n={n}")
    if result is None:
        sys.exit(1)
    if as_json:
        click.echo(json.dumps(result, indent=2))
        return
    if not result:
        click.echo("No audit entries.")
        return
    for entry in result:
        outcome_color = "green" if entry.get("outcome") == "SUCCESS" else "red"
        outcome = click.style(entry.get("outcome", "?"), fg=outcome_color)
        click.echo(
            f"  {entry.get('ts', '?')[:19]}  {entry.get('token_id', '?'):8s}  "
            f"{entry.get('event', '?'):15s}  {entry.get('target', '?'):40s}  "
            f"{outcome}  {entry.get('latency_ms', 0):.1f}ms"
        )


@mcp_group.command("stdio")
@click.option("--drone", default="localhost", help="Drone host or IP.")
@click.option("--port", default=8090, help="MCP HTTP port.")
def mcp_stdio(drone: str, port: int) -> None:
    """Start a stdio-to-HTTP+SSE bridge for external MCP clients.

    This command wraps the drone's MCP HTTP server with a stdio interface
    that Claude Desktop, Cursor, VS Code, and other MCP clients can use
    by adding it to their MCP server config as a subprocess.

    Usage in Claude Desktop config:
      {
        "mcpServers": {
          "ados-drone": {
            "command": "ados",
            "args": ["mcp", "stdio", "--drone", "drone-hostname.local"]
          }
        }
      }
    """
    try:
        from mcp.client.stdio import stdio_client
        from mcp import ClientSession
        import anyio
    except ImportError:
        click.echo("Error: mcp package not found. Install with: pip install mcp", err=True)
        sys.exit(1)

    # Run the stdio bridge using the MCP SDK's built-in mechanism.
    # The drone's HTTP+SSE transport is wrapped in a stdio interface.
    base_url = f"http://{drone}:{port}/mcp"
    click.echo(f"Connecting to {base_url} via stdio...", err=True)

    try:
        from mcp.client.sse import sse_client
        import anyio

        async def run_bridge() -> None:
            async with sse_client(base_url) as (read, write):
                async with ClientSession(read, write) as session:
                    await session.initialize()
                    # Keep alive until stdin closes
                    await anyio.sleep_forever()

        anyio.run(run_bridge)
    except Exception as e:
        click.echo(f"Stdio bridge error: {e}", err=True)
        sys.exit(1)


@mcp_group.command("test")
@click.argument("tool_name")
@click.argument("args_json", required=False, default="{}")
@click.option("--json", "as_json", is_flag=True, default=True, help="Output raw JSON.")
def mcp_test(tool_name: str, args_json: str, as_json: bool) -> None:
    """Call a single MCP Tool by name with JSON args and print the result.

    Example:
      ados mcp test agent.health
      ados mcp test flight.arm '{"simulate": true}'
    """
    try:
        args = json.loads(args_json)
    except json.JSONDecodeError as e:
        click.echo(f"Error parsing args JSON: {e}", err=True)
        sys.exit(1)

    # Call via the MCP HTTP endpoint directly
    try:
        resp = httpx.post(
            f"http://127.0.0.1:8090/mcp",
            json={
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": tool_name, "arguments": args},
                "id": 1,
            },
            headers={**_agent_key(), "Content-Type": "application/json"},
            timeout=10.0,
        )
        result = resp.json()
        click.echo(json.dumps(result, indent=2))
    except Exception as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)
