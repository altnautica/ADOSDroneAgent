"""AsciiHeader widget — compact ADOS ASCII art banner."""

from __future__ import annotations

from textual.widgets import Static

_BANNER = """\
[#3a82ff]    _   ___   ___  ___
   /_\\ |   \\ / _ \\/ __|
  / _ \\| |) | (_) \\__ \\
 /_/ \\_\\___/ \\___/|___/[/#3a82ff]"""


class AsciiHeader(Static):
    """Compact ASCII art header for ADOS branding."""

    DEFAULT_CSS = """
    AsciiHeader {
        height: auto;
        padding: 0;
        margin: 0;
        text-align: center;
        background: transparent;
    }
    """

    def __init__(
        self,
        *,
        name: str | None = None,
        id: str | None = None,
        classes: str | None = None,
    ) -> None:
        super().__init__(_BANNER, name=name, id=id, classes=classes)
