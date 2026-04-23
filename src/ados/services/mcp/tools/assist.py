"""MCP Assist tool handlers — stubs until Phase 7."""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP

_ASSIST_TOOLS = [
    "assist.diagnose", "assist.suggest_for", "assist.subscribe_diagnostics",
    "assist.acknowledge_suggestion", "assist.dismiss_suggestion",
    "assist.get_status", "assist.set_features", "assist.set_scope",
    "repair.list_pending", "repair.approve", "repair.execute",
    "repair.rollback", "repair.audit_log", "repair.cancel",
    "pr.draft", "pr.preview_diff", "pr.list_drafts",
    "pr.push", "pr.cancel", "pr.list_open",
    "setup.start_wizard", "setup.next_step", "setup.previous_step",
    "setup.complete", "setup.list_wizards",
    "fleet.detect_patterns", "fleet.suggest_fix_for_pattern",
    "fleet.list_patterns", "fleet.get_pattern_detail",
    "assist.opt_in.enable_feature", "assist.opt_in.disable_feature",
    "assist.opt_in.set_safety_scope", "assist.opt_in.list_enabled",
]


def register(mcp: FastMCP) -> None:
    """Register Assist tools as stubs on the MCP server."""
    for name in _ASSIST_TOOLS:
        _register_stub(mcp, name)


def _register_stub(mcp: FastMCP, name: str) -> None:
    @mcp.tool(name=name)
    async def _stub(**kwargs: object) -> dict:
        return {
            "status": "not_implemented",
            "message": "Assist service not yet active. Enables in a future update.",
        }
    _stub.__name__ = name.replace(".", "_")
    _stub.__qualname__ = name.replace(".", "_")
