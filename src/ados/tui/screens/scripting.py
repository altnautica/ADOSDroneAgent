"""Scripting TUI screen — running scripts, command log, engine status."""

from __future__ import annotations

import httpx
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import DataTable, Static

API = "http://localhost:8080"


class ScriptingScreen(Screen):
    """Scripting engine overview — scripts table, command log, safety status."""

    def compose(self) -> ComposeResult:
        with Horizontal():
            with Vertical(id="left-col"):
                yield Static("[b]Running Scripts[/b]", classes="panel-title")
                yield DataTable(id="scripts-table")
                yield Static("[b]Engine Status[/b]", classes="panel-title")
                yield Static("Loading...", id="status-panel")
            with Vertical(id="right-col"):
                yield Static("[b]Command Log[/b]", classes="panel-title")
                yield Static("Loading...", id="log-panel")

    def on_mount(self) -> None:
        table = self.query_one("#scripts-table", DataTable)
        table.add_columns("ID", "File", "State", "PID")
        self.set_interval(1.0, self._refresh)

    async def _refresh(self) -> None:
        try:
            async with httpx.AsyncClient(timeout=3.0) as client:
                scripts_resp = await client.get(f"{API}/api/scripts")
                scripts_data = scripts_resp.json()

                status_resp = await client.get(f"{API}/api/scripting/status")
                status_data = status_resp.json()
        except Exception:
            self.query_one("#status-panel", Static).update("Agent not running")
            return

        # Scripts table
        table = self.query_one("#scripts-table", DataTable)
        table.clear()
        for s in scripts_data.get("scripts", []):
            table.add_row(
                s.get("script_id", "")[:8],
                s.get("filename", ""),
                s.get("state", ""),
                str(s.get("pid", "")),
            )

        # Command log
        entries = scripts_data.get("command_log", [])
        lines = []
        for e in entries[-15:]:
            ts = e.get("timestamp", "")[-8:]  # just time portion
            cmd = e.get("command", "")[:30]
            src = e.get("source", "")
            res = e.get("result", "")[:20]
            lines.append(f"{ts} [{src}] {cmd} -> {res}")
        self.query_one("#log-panel", Static).update("\n".join(lines) or "No commands yet")

        # Status
        demo = status_data.get("demo_mode", False)
        fc = status_data.get("fc_connected", False)
        cmd_count = status_data.get("commands_executed", 0)
        scripts_running = status_data.get("scripts_running", 0)
        scripts_total = status_data.get("scripts_total", 0)

        mode_str = "DEMO" if demo else ("LIVE" if fc else "DISCONNECTED")
        status_lines = [
            f"Mode:     {mode_str}",
            f"FC:       {'Connected' if fc else 'Not connected'}",
            f"Commands: {cmd_count}",
            f"Scripts:  {scripts_running} running / {scripts_total} total",
        ]

        if demo:
            status_lines.append(f"Alt:      {status_data.get('altitude', 0.0):.1f}m")
            status_lines.append(f"Armed:    {status_data.get('armed', False)}")
            status_lines.append(f"FlightM:  {status_data.get('mode', 'N/A')}")

        self.query_one("#status-panel", Static).update("\n".join(status_lines))
