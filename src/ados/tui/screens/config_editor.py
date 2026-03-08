"""Config editor screen — view and edit YAML configuration."""

from __future__ import annotations

from pathlib import Path

import structlog
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import Button, Static, TextArea

log = structlog.get_logger("tui.config_editor")

CONFIG_PATHS = [
    Path("/etc/ados/config.yaml"),
    Path("config.yaml"),
    Path("configs/config.example.yaml"),
]


class ConfigScreen(Screen):
    """YAML config editor."""

    def __init__(self) -> None:
        super().__init__()
        self._config_path: Path | None = None

    def compose(self) -> ComposeResult:
        with Vertical():
            yield Static("[b]Configuration Editor[/b]", classes="panel-title")
            yield Static("", id="config-path")
            yield TextArea("", id="config-editor", language="yaml")
            with Horizontal():
                yield Button("Save", id="btn-save", variant="primary")
                yield Button("Reload", id="btn-reload", variant="default")

    def on_mount(self) -> None:
        self._load_config()

    def _load_config(self) -> None:
        editor = self.query_one("#config-editor", TextArea)
        path_label = self.query_one("#config-path", Static)

        for p in CONFIG_PATHS:
            try:
                if p.is_file():
                    self._config_path = p
                    content = p.read_text()
                    editor.load_text(content)
                    path_label.update(f"File: {p}")
                    return
            except OSError as exc:
                log.warning("config_load_failed", path=str(p), error=str(exc))

        editor.load_text("# No config file found")
        path_label.update("File: (none)")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "btn-save":
            self._save_config()
        elif event.button.id == "btn-reload":
            self._load_config()

    def _save_config(self) -> None:
        if not self._config_path:
            return
        editor = self.query_one("#config-editor", TextArea)
        try:
            self._config_path.write_text(editor.text)
            path_label = self.query_one("#config-path", Static)
            path_label.update(f"File: {self._config_path} [saved]")
        except OSError as e:
            log.warning("config_save_failed", path=str(self._config_path), error=str(e))
            path_label = self.query_one("#config-path", Static)
            path_label.update(f"Error: {e}")
