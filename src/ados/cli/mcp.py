"""`ados mcp ...` subcommands for the MCP server.

Provides pairing, token management, audit tailing, and a stdio bridge
for external MCP clients (Claude Desktop, Cursor, VS Code, custom scripts).

Registered under the main `cli` group in `ados.cli.main` via
`cli.add_command(mcp_group)`.
"""

from __future__ import annotations

import click

API_BASE = "http://127.0.0.1:8080"


# ---------------------------------------------------------------------------
# Group
# ---------------------------------------------------------------------------

@click.group("mcp")
def mcp_group() -> None:
    """Manage the on-drone MCP server: pairing, tokens, audit, stdio bridge."""


# ---------------------------------------------------------------------------
# Commands (stubs — full implementation in Phase 1)
# ---------------------------------------------------------------------------

@mcp_group.command("pair")
def mcp_pair() -> None:
    """Mint a new session token and display the 6-word pairing mnemonic."""
    click.echo("mcp pair: not yet implemented (Phase 1)")


@mcp_group.command("status")
def mcp_status() -> None:
    """Show MCP service health, active sessions, and subscription counts."""
    click.echo("mcp status: not yet implemented (Phase 1)")


@mcp_group.command("tokens")
@click.argument("action", type=click.Choice(["list", "revoke"]))
@click.argument("token_id", required=False)
def mcp_tokens(action: str, token_id: str | None) -> None:
    """List or revoke session tokens. Use 'list' or 'revoke <token_id>'."""
    click.echo(f"mcp tokens {action}: not yet implemented (Phase 1)")


@mcp_group.command("audit")
@click.option("--n", default=50, help="Number of recent entries to show.")
def mcp_audit(n: int) -> None:
    """Tail the MCP audit log (most recent N entries)."""
    click.echo(f"mcp audit tail --n {n}: not yet implemented (Phase 1)")


@mcp_group.command("stdio")
@click.option("--drone", default="localhost", help="Drone host or IP.")
def mcp_stdio(drone: str) -> None:
    """Start a stdio-to-HTTP+SSE bridge for Claude Desktop, Cursor, or VS Code."""
    click.echo(f"mcp stdio --drone {drone}: not yet implemented (Phase 1)")


@mcp_group.command("test")
@click.argument("tool_name")
@click.argument("args_json", required=False, default="{}")
def mcp_test(tool_name: str, args_json: str) -> None:
    """Call a single MCP Tool by name with JSON args and print the result."""
    click.echo(f"mcp test {tool_name}: not yet implemented (Phase 1)")
