"""Scripting TUI screen — running scripts, command log, engine status."""

from __future__ import annotations

import structlog
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import DataTable, RichLog, Static

from ados.tui.widgets import InfoPanel, StatusDot

log = structlog.get_logger("tui.scripting")


class ScriptingScreen(Screen):
    """Scripting engine overview — scripts table, command log, safety status."""

    def compose(self) -> ComposeResult:
        with Horizontal():
            with Vertical(id="left-col"):
                with InfoPanel("ENGINE"):
                    yield StatusDot("Mode", "unknown", id="engine-mode-dot")
                    yield StatusDot("FC", "disconnected", id="fc-dot")
                    yield Static("", id="engine-stats")
                with InfoPanel("RUNNING SCRIPTS"):
                    yield DataTable(id="scripts-table")
            with Vertical(id="right-col"):
                with InfoPanel("COMMAND LOG"):
                    yield RichLog(id="cmd-log", auto_scroll=True, max_lines=200)

    def on_mount(self) -> None:
        table = self.query_one("#scripts-table", DataTable)
        table.add_columns("ID", "File", "State", "PID")
        self.set_interval(1.0, self._refresh)

    async def _refresh(self) -> None:
        fetcher = self.app.fetcher  # type: ignore[attr-defined]
        scripts_data = await fetcher.get_scripts()
        status_data = await fetcher.get_scripting_status()

        mode_dot = self.query_one("#engine-mode-dot", StatusDot)
        fc_dot = self.query_one("#fc-dot", StatusDot)

        if scripts_data is None or status_data is None:
            mode_dot.set_state("disconnected")
            fc_dot.set_state("disconnected")
            return

        # Engine status dots
        demo = status_data.get("demo_mode", False)
        fc = status_data.get("fc_connected", False)

        if demo:
            mode_dot.set_state("warning")
            mode_dot._label = "Mode"
            mode_dot._state = "warning"
            # Manually render with custom label
            color = "#f59e0b"
            mode_dot.update(f"[{color}]\u25cf[/{color}] Mode: [{color}]DEMO[/{color}]")
        elif fc:
            mode_dot.set_state("connected")
            color = "#22c55e"
            mode_dot.update(f"[{color}]\u25cf[/{color}] Mode: [{color}]LIVE[/{color}]")
        else:
            mode_dot.set_state("disconnected")
            color = "#ef4444"
            mode_dot.update(f"[{color}]\u25cf[/{color}] Mode: [{color}]DISCONNECTED[/{color}]")

        fc_dot.set_state("connected" if fc else "disconnected")

        # Engine stats
        cmd_count = status_data.get("commands_executed", 0)
        scripts_running = status_data.get("scripts_running", 0)
        scripts_total = status_data.get("scripts_total", 0)

        stats_lines = [
            f"Commands: {cmd_count}",
            f"Scripts:  {scripts_running} running / {scripts_total} total",
        ]
        if demo:
            stats_lines.append(f"Alt:      {status_data.get('altitude', 0.0):.1f}m")
            stats_lines.append(f"Armed:    {status_data.get('armed', False)}")
            stats_lines.append(f"FlightM:  {status_data.get('mode', 'N/A')}")

        self.query_one("#engine-stats", Static).update("\n".join(stats_lines))

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

        # Command log (RichLog for styled output)
        cmd_log = self.query_one("#cmd-log", RichLog)
        entries = scripts_data.get("command_log", [])
        # Only write new entries (RichLog appends, so we clear and rewrite)
        cmd_log.clear()
        for e in entries[-25:]:
            ts = e.get("timestamp", "")[-8:]
            cmd = e.get("command", "")[:30]
            src = e.get("source", "")
            res = e.get("result", "")[:20]
            # Color-code: sdk source = blue, tello = lime, error results = red
            if "error" in res.lower() or "fail" in res.lower():
                cmd_log.write(f"[red]{ts}[/red] [{src}] {cmd} -> [red]{res}[/red]")
            elif src == "sdk":
                cmd_log.write(f"[#3a82ff]{ts}[/#3a82ff] [{src}] {cmd} -> {res}")
            elif src == "tello":
                cmd_log.write(f"[#dff140]{ts}[/#dff140] [{src}] {cmd} -> {res}")
            else:
                cmd_log.write(f"[dim]{ts}[/dim] [{src}] {cmd} -> {res}")
