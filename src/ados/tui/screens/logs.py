"""Log viewer screen — filterable, searchable log display."""

from __future__ import annotations

import re

import httpx
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import Button, Input, RichLog, Static

API = "http://localhost:8080"


class LogsScreen(Screen):
    """Real-time log viewer with filters."""

    def __init__(self) -> None:
        super().__init__()
        self._level_filter: str | None = None
        self._search_pattern: str = ""
        self._seen_count: int = 0

    def compose(self) -> ComposeResult:
        with Vertical():
            yield Static("[b]Log Viewer[/b]", classes="panel-title")
            with Horizontal(id="log-filters"):
                yield Button("ALL", id="btn-all", variant="primary")
                yield Button("DEBUG", id="btn-debug")
                yield Button("INFO", id="btn-info")
                yield Button("WARN", id="btn-warn")
                yield Button("ERROR", id="btn-error")
                yield Input(placeholder="Search (regex)...", id="log-search")
            yield RichLog(id="log-output", auto_scroll=True, max_lines=500)

    def on_mount(self) -> None:
        self.set_interval(1.0, self._refresh)

    def on_button_pressed(self, event: Button.Pressed) -> None:
        btn_id = event.button.id
        if btn_id == "btn-all":
            self._level_filter = None
        elif btn_id == "btn-debug":
            self._level_filter = "DEBUG"
        elif btn_id == "btn-info":
            self._level_filter = "INFO"
        elif btn_id == "btn-warn":
            self._level_filter = "WARNING"
        elif btn_id == "btn-error":
            self._level_filter = "ERROR"

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id == "log-search":
            self._search_pattern = event.value

    async def _refresh(self) -> None:
        try:
            params = {"limit": 100, "offset": self._seen_count}
            if self._level_filter:
                params["level"] = self._level_filter
            async with httpx.AsyncClient(timeout=3.0) as client:
                resp = await client.get(f"{API}/api/logs", params=params)
                data = resp.json()
        except Exception:
            return

        log_widget = self.query_one("#log-output", RichLog)
        entries = data.get("entries", [])

        for entry in entries:
            msg = entry.get("message", "")
            level = entry.get("level", "INFO")

            # Apply search filter
            if self._search_pattern:
                try:
                    if not re.search(self._search_pattern, msg, re.IGNORECASE):
                        continue
                except re.error:
                    pass

            # Color by level
            if level == "ERROR":
                log_widget.write(f"[red][{level}][/red] {msg}")
            elif level == "WARNING":
                log_widget.write(f"[yellow][{level}][/yellow] {msg}")
            elif level == "DEBUG":
                log_widget.write(f"[dim][{level}][/dim] {msg}")
            else:
                log_widget.write(f"[{level}] {msg}")

        self._seen_count += len(entries)
