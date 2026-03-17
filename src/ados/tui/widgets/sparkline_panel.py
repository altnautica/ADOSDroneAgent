"""SparklinePanel widget — Sparkline with title and current value."""

from __future__ import annotations

from collections import deque

from textual.app import ComposeResult
from textual.containers import Vertical
from textual.widgets import Sparkline, Static


class SparklinePanel(Vertical):
    """A sparkline graph with a title header and current value display.

    Parameters
    ----------
    title:
        Panel heading text.
    maxlen:
        Ring buffer size for sparkline data.
    unit:
        Unit suffix for the current value (e.g. ``m``, ``m/s``).
    """

    DEFAULT_CSS = """
    SparklinePanel {
        height: auto;
        padding: 0;
        margin: 0 0 1 0;
        background: transparent;
    }
    SparklinePanel > .sparkline-title {
        height: 1;
        padding: 0;
        margin: 0;
        color: #3a82ff;
        text-style: bold;
        background: transparent;
    }
    SparklinePanel > .sparkline-value {
        height: 1;
        padding: 0;
        margin: 0;
        color: #fafafa;
        background: transparent;
    }
    SparklinePanel Sparkline {
        height: 3;
        margin: 0;
        padding: 0;
    }
    """

    def __init__(
        self,
        title: str = "",
        maxlen: int = 60,
        unit: str = "",
        *,
        name: str | None = None,
        id: str | None = None,
        classes: str | None = None,
    ) -> None:
        super().__init__(name=name, id=id, classes=classes)
        self._title = title
        self._unit = unit
        self._buffer: deque[float] = deque(maxlen=maxlen)
        self._spark_id = f"_spark_{id or 'x'}"
        self._val_id = f"_val_{id or 'x'}"

    def compose(self) -> ComposeResult:
        yield Static(f"[#3a82ff]{self._title}[/#3a82ff]", classes="sparkline-title")
        yield Static("--", id=self._val_id, classes="sparkline-value")
        yield Sparkline([], id=self._spark_id)

    def push(self, value: float) -> None:
        """Add a sample and update the display."""
        self._buffer.append(value)
        data = list(self._buffer)
        try:
            spark = self.query_one(f"#{self._spark_id}", Sparkline)
            spark.data = data
        except Exception:
            pass
        try:
            val_widget = self.query_one(f"#{self._val_id}", Static)
            val_widget.update(f"  {value:.1f} {self._unit}")
        except Exception:
            pass

    def set_data(self, data: list[float]) -> None:
        """Replace the buffer with external data and update."""
        self._buffer.clear()
        self._buffer.extend(data)
        display = list(self._buffer)
        try:
            spark = self.query_one(f"#{self._spark_id}", Sparkline)
            spark.data = display
        except Exception:
            pass
        if display:
            try:
                val_widget = self.query_one(f"#{self._val_id}", Static)
                val_widget.update(f"  {display[-1]:.1f} {self._unit}")
            except Exception:
                pass
