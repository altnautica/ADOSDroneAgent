"""SatelliteBar widget — visual dot array showing satellite count."""

from __future__ import annotations

from textual.widgets import Static


class SatelliteBar(Static):
    """Visual satellite count: ``Sats ●●●●●●○○○○ 6/10``

    Parameters
    ----------
    max_sats:
        Maximum number of dots to display.
    """

    DEFAULT_CSS = """
    SatelliteBar {
        height: 1;
        padding: 0;
        margin: 0;
        background: transparent;
    }
    """

    def __init__(
        self,
        max_sats: int = 20,
        *,
        name: str | None = None,
        id: str | None = None,
        classes: str | None = None,
    ) -> None:
        super().__init__("", name=name, id=id, classes=classes)
        self._max_sats = max_sats
        self._count = 0
        self._render_bar()

    def update_count(self, count: int) -> None:
        """Update the satellite count and re-render."""
        self._count = max(0, min(self._max_sats, count))
        self._render_bar()

    def _render_bar(self) -> None:
        filled = "\u25cf" * self._count       # ●
        empty = "\u25cb" * (self._max_sats - self._count)  # ○

        if self._count >= 8:
            color = "#22c55e"
        elif self._count >= 5:
            color = "#f59e0b"
        else:
            color = "#ef4444"

        self.update(
            f"[#3a82ff]Sats[/#3a82ff] "
            f"[{color}]{filled}[/{color}]"
            f"[#333333]{empty}[/#333333]"
            f" [{color}]{self._count}/{self._max_sats}[/{color}]"
        )
