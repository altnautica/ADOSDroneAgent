"""GaugeBar widget — horizontal bar with Unicode blocks and color thresholds."""

from __future__ import annotations

from textual.widgets import Static


class GaugeBar(Static):
    """Horizontal gauge bar: ``CPU [████████░░░░░░░░] 43%``

    Parameters
    ----------
    label:
        Text label shown before the bar.
    value:
        Initial value (0-100).
    bar_width:
        Number of character cells for the bar.
    thresholds:
        Tuple of (warn, error) values. Below warn = green,
        warn-error = yellow, above error = red.
    suffix:
        Text appended after the value (default ``%``).
    """

    DEFAULT_CSS = """
    GaugeBar {
        height: 1;
        padding: 0;
        margin: 0;
        background: transparent;
    }
    """

    def __init__(
        self,
        label: str = "",
        value: float = 0.0,
        bar_width: int = 16,
        thresholds: tuple[float, float] = (60.0, 85.0),
        suffix: str = "%",
        *,
        name: str | None = None,
        id: str | None = None,
        classes: str | None = None,
    ) -> None:
        super().__init__("", name=name, id=id, classes=classes)
        self._label = label
        self._value = value
        self._bar_width = bar_width
        self._thresholds = thresholds
        self._suffix = suffix
        self._render_bar()

    def update_value(self, value: float) -> None:
        """Update the gauge value and re-render."""
        self._value = max(0.0, min(100.0, value))
        self._render_bar()

    def _render_bar(self) -> None:
        v = max(0.0, min(100.0, self._value))
        filled = int(v / 100.0 * self._bar_width)
        filled = max(0, min(self._bar_width, filled))

        warn_thresh, err_thresh = self._thresholds

        if v >= err_thresh:
            color = "#ef4444"
        elif v >= warn_thresh:
            color = "#f59e0b"
        else:
            color = "#22c55e"

        bar_filled = "\u2588" * filled
        bar_empty = "\u2591" * (self._bar_width - filled)

        label_part = f"{self._label:<6}" if self._label else ""
        text = (
            f"{label_part}"
            f"[{color}]{bar_filled}[/{color}]"
            f"[#333333]{bar_empty}[/#333333]"
            f" [{color}]{v:5.1f}{self._suffix}[/{color}]"
        )
        self.update(text)
