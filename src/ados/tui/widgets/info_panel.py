"""InfoPanel widget — bordered container with a colored title."""

from __future__ import annotations

from textual.app import ComposeResult
from textual.containers import Vertical
from textual.widgets import Static


class InfoPanel(Vertical):
    """Bordered container with a title in accent blue.

    Usage::

        with InfoPanel("SYSTEM"):
            yield GaugeBar(...)
            yield StatusDot(...)
    """

    DEFAULT_CSS = """
    InfoPanel {
        border: solid #1a1a2e;
        padding: 1;
        margin: 0 1 1 0;
        background: #0a0a0a;
        height: auto;
    }
    InfoPanel > .info-panel--title {
        color: #3a82ff;
        text-style: bold;
        height: 1;
        padding: 0;
        margin: 0 0 1 0;
        background: transparent;
    }
    """

    def __init__(
        self,
        title: str = "",
        *,
        name: str | None = None,
        id: str | None = None,
        classes: str | None = None,
    ) -> None:
        super().__init__(name=name, id=id, classes=classes)
        self._title = title

    def compose(self) -> ComposeResult:
        if self._title:
            yield Static(
                f"[#3a82ff]{self._title}[/#3a82ff]",
                classes="info-panel--title",
            )
