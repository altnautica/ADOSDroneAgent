"""ADOS Drone Agent TUI — Textual-based dashboard."""

from __future__ import annotations

from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.widgets import Footer, Header

from ados import __version__
from ados.tui.screens.config_editor import ConfigScreen
from ados.tui.screens.dashboard import DashboardScreen
from ados.tui.screens.link import LinkScreen
from ados.tui.screens.logs import LogsScreen
from ados.tui.screens.mavlink import MavlinkScreen
from ados.tui.screens.scripting import ScriptingScreen
from ados.tui.screens.telemetry import TelemetryScreen
from ados.tui.screens.updates import UpdatesScreen
from ados.tui.screens.video import VideoScreen

_DEFAULT_API_URL = "http://localhost:8080"


class ADOSTui(App):
    """ADOS Drone Agent Terminal User Interface."""

    TITLE = f"ADOS Drone Agent v{__version__}"
    CSS = """
    Screen {
        background: #0a0a0f;
        color: #e0e0e0;
    }
    Header {
        background: #111118;
        color: #3a82ff;
    }
    Footer {
        background: #111118;
    }
    DataTable {
        background: #0f0f18;
    }
    DataTable > .datatable--header {
        background: #1a1a28;
        color: #3a82ff;
    }
    Static {
        background: #0f0f18;
        padding: 1;
        margin: 0 1;
    }
    .panel-title {
        color: #3a82ff;
        text-style: bold;
    }
    .value-good {
        color: #dff140;
    }
    .value-warn {
        color: #ffaa00;
    }
    .value-bad {
        color: #ff4444;
    }
    """

    BINDINGS = [
        Binding("d", "switch_screen('dashboard')", "Dashboard"),
        Binding("t", "switch_screen('telemetry')", "Telemetry"),
        Binding("m", "switch_screen('mavlink')", "MAVLink"),
        Binding("v", "switch_screen('video')", "Video"),
        Binding("w", "switch_screen('link')", "Link"),
        Binding("s", "switch_screen('scripting')", "Script"),
        Binding("u", "switch_screen('updates')", "Updates"),
        Binding("c", "switch_screen('config')", "Config"),
        Binding("l", "switch_screen('logs')", "Logs"),
        Binding("q", "quit", "Quit"),
    ]

    SCREENS = {
        "dashboard": DashboardScreen,
        "telemetry": TelemetryScreen,
        "mavlink": MavlinkScreen,
        "video": VideoScreen,
        "link": LinkScreen,
        "scripting": ScriptingScreen,
        "updates": UpdatesScreen,
        "config": ConfigScreen,
        "logs": LogsScreen,
    }

    def __init__(self, api_url: str | None = None) -> None:
        super().__init__()
        if api_url is not None:
            self.api_url = api_url
        else:
            # Try reading from config
            try:
                from ados.core.config import load_config
                cfg = load_config()
                port = cfg.scripting.rest_api.port
                self.api_url = f"http://localhost:{port}"
            except Exception:
                self.api_url = _DEFAULT_API_URL

    def compose(self) -> ComposeResult:
        yield Header()
        yield Footer()

    def on_mount(self) -> None:
        self.push_screen("dashboard")

    def action_switch_screen(self, screen_name: str) -> None:
        # Pop current screen and push new one
        if self.screen_stack:
            self.pop_screen()
        self.push_screen(screen_name)
