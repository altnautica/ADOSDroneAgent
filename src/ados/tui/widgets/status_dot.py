"""StatusDot widget — colored circle indicator with label."""

from __future__ import annotations

from textual.widgets import Static

# State-to-color mapping
_STATE_COLORS: dict[str, str] = {
    "connected": "#22c55e",
    "running": "#22c55e",
    "active": "#22c55e",
    "paired": "#22c55e",
    "ok": "#22c55e",
    "armed": "#dff140",
    "ready": "#dff140",
    "starting": "#f59e0b",
    "warning": "#f59e0b",
    "connecting": "#f59e0b",
    "degraded": "#f59e0b",
    "stopped": "#ef4444",
    "error": "#ef4444",
    "failed": "#ef4444",
    "disconnected": "#ef4444",
    "unpaired": "#ef4444",
    "disabled": "#666666",
    "idle": "#666666",
    "unknown": "#666666",
}


class StatusDot(Static):
    """Colored dot indicator with a label.

    Usage::

        StatusDot("MAVLink", "connected")
        StatusDot("Armed", "armed")
    """

    DEFAULT_CSS = """
    StatusDot {
        height: 1;
        padding: 0;
        margin: 0;
        background: transparent;
    }
    """

    def __init__(
        self,
        label: str = "",
        state: str = "unknown",
        *,
        name: str | None = None,
        id: str | None = None,
        classes: str | None = None,
    ) -> None:
        super().__init__("", name=name, id=id, classes=classes)
        self._label = label
        self._state = state
        self._render()

    def set_state(self, state: str) -> None:
        """Update the dot state and re-render."""
        self._state = state
        self._render()

    def _render(self) -> None:
        color = _STATE_COLORS.get(self._state, "#666666")
        dot = "\u25cf"  # ●
        self.update(f"[{color}]{dot}[/{color}] {self._label}: [{color}]{self._state}[/{color}]")
