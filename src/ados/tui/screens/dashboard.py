"""Dashboard screen — services, system resources, FC info, recent logs."""

from __future__ import annotations

import httpx
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import DataTable, Static

API = "http://localhost:8080"


class DashboardScreen(Screen):
    """Main dashboard with service table, system health, FC info."""

    def compose(self) -> ComposeResult:
        with Horizontal():
            with Vertical(id="left-col"):
                yield Static("[b]Services[/b]", classes="panel-title")
                yield DataTable(id="services-table")
                yield Static("[b]Recent Logs[/b]", classes="panel-title")
                yield Static("Loading...", id="logs-panel")
            with Vertical(id="right-col"):
                yield Static("[b]System Health[/b]", classes="panel-title")
                yield Static("Loading...", id="health-panel")
                yield Static("[b]Flight Controller[/b]", classes="panel-title")
                yield Static("Loading...", id="fc-panel")

    def on_mount(self) -> None:
        table = self.query_one("#services-table", DataTable)
        table.add_columns("Service", "Status")
        self.set_interval(1.0, self._refresh)

    async def _refresh(self) -> None:
        try:
            async with httpx.AsyncClient(timeout=3.0) as client:
                status_resp = await client.get(f"{API}/api/status")
                status = status_resp.json()

                services_resp = await client.get(f"{API}/api/services")
                services = services_resp.json()

                logs_resp = await client.get(f"{API}/api/logs?limit=10")
                logs = logs_resp.json()
        except Exception:
            self.query_one("#health-panel", Static).update("Agent not running")
            return

        # Health
        h = status.get("health", {})
        health_text = (
            f"CPU:    {h.get('cpu_percent', 0):.1f}%\n"
            f"Memory: {h.get('memory_percent', 0):.1f}%\n"
            f"Disk:   {h.get('disk_percent', 0):.1f}%\n"
            f"Temp:   {h.get('temperature', 'N/A')}"
        )
        self.query_one("#health-panel", Static).update(health_text)

        # FC
        fc_text = (
            f"Connected: {status.get('fc_connected', False)}\n"
            f"Port:      {status.get('fc_port', 'N/A')}\n"
            f"Baud:      {status.get('fc_baud', 'N/A')}\n"
            f"Board:     {status.get('board', {}).get('name', '?')}\n"
            f"Tier:      {status.get('board', {}).get('tier', '?')}\n"
            f"Uptime:    {status.get('uptime_seconds', 0):.0f}s"
        )
        self.query_one("#fc-panel", Static).update(fc_text)

        # Services table
        table = self.query_one("#services-table", DataTable)
        table.clear()
        for svc in services.get("services", []):
            table.add_row(svc["name"], svc["status"])

        # Logs
        entries = logs.get("entries", [])
        lines = []
        for e in entries[-10:]:
            lines.append(f"[{e.get('level', '?')}] {e.get('message', '')[:80]}")
        self.query_one("#logs-panel", Static).update("\n".join(lines) or "No logs")
