"""Log viewer screen — filterable, searchable log display."""

from __future__ import annotations

import re

import httpx
import structlog
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import Button, Input, RichLog

from ados.tui.widgets import InfoPanel

_log = structlog.get_logger("tui.logs")

# Level button styling: active variant is "primary", inactive is "default"
_LEVEL_BUTTONS = {
    "btn-all": None,
    "btn-debug": "DEBUG",
    "btn-info": "INFO",
    "btn-warn": "WARNING",
    "btn-error": "ERROR",
}


class LogsScreen(Screen):
    """Real-time log viewer with filters."""

    def __init__(self) -> None:
        super().__init__()
        self._level_filter: str | None = None
        self._search_pattern: str = ""
        self._seen_count: int = 0
        self._active_btn: str = "btn-all"

    def compose(self) -> ComposeResult:
        with Vertical():
            with InfoPanel("LOG VIEWER"):
                with Horizontal(id="log-filters"):
                    yield Button("ALL", id="btn-all", variant="primary")
                    yield Button("DEBUG", id="btn-debug", variant="default")
                    yield Button("INFO", id="btn-info", variant="default")
                    yield Button("WARN", id="btn-warn", variant="default")
                    yield Button("ERROR", id="btn-error", variant="default")
                yield Input(placeholder="Search (regex)...", id="log-search")
            with InfoPanel("OUTPUT"):
                yield RichLog(id="log-output", auto_scroll=True, max_lines=500)

    def on_mount(self) -> None:
        self.set_interval(1.0, self._refresh)

    def on_button_pressed(self, event: Button.Pressed) -> None:
        btn_id = event.button.id or ""
        if btn_id not in _LEVEL_BUTTONS:
            return

        self._level_filter = _LEVEL_BUTTONS[btn_id]

        # Highlight the active filter button, dim the rest
        for bid in _LEVEL_BUTTONS:
            try:
                btn = self.query_one(f"#{bid}", Button)
                btn.variant = "primary" if bid == btn_id else "default"
            except Exception:
                pass
        self._active_btn = btn_id

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id == "log-search":
            self._search_pattern = event.value

    async def _refresh(self) -> None:
        # Fetcher's get_logs only takes limit, but we need offset and level params.
        # Keep direct httpx for this endpoint.
        api = self.app.api_url  # type: ignore[attr-defined]
        try:
            params: dict[str, int | str] = {"limit": 100, "offset": self._seen_count}
            if self._level_filter:
                params["level"] = self._level_filter
            async with httpx.AsyncClient(timeout=3.0) as client:
                resp = await client.get(f"{api}/api/logs", params=params)
                data = resp.json()
        except httpx.ConnectError:
            log_widget = self.query_one("#log-output", RichLog)
            if self._seen_count == 0:
                log_widget.write(
                    "Agent not running.\n"
                    "Start with: ados demo    (simulated)\n"
                    "       or:  ados start   (real FC)"
                )
                self._seen_count = -1  # prevent repeat
            return
        except Exception as exc:
            _log.warning("logs_refresh_failed", error=str(exc))
            return

        log_widget = self.query_one("#log-output", RichLog)
        entries = data.get("entries", [])

        for entry in entries:
            msg = entry.get("message", "")
            level = entry.get("level", "INFO")
            ts = entry.get("timestamp", "")[-8:]  # time portion

            # Apply search filter
            if self._search_pattern:
                try:
                    if not re.search(self._search_pattern, msg, re.IGNORECASE):
                        continue
                except re.error:
                    pass

            # Color-coded by level with timestamp prefix
            if level == "ERROR":
                log_widget.write(f"[red]{ts} [{level}] {msg}[/red]")
            elif level == "WARNING":
                log_widget.write(f"[yellow]{ts} [{level}] {msg}[/yellow]")
            elif level == "DEBUG":
                log_widget.write(f"[dim]{ts} [{level}] {msg}[/dim]")
            elif level == "INFO":
                log_widget.write(f"[#3a82ff]{ts}[/#3a82ff] [{level}] {msg}")
            else:
                log_widget.write(f"{ts} [{level}] {msg}")

        self._seen_count += len(entries)
