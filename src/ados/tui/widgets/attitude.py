"""AttitudeIndicator widget — ASCII artificial horizon display."""

from __future__ import annotations

import math

from textual.widgets import Static

_WIDTH = 25
_HEIGHT = 11


class AttitudeIndicator(Static):
    """ASCII artificial horizon showing roll and pitch.

    Renders an ~11-line, ~25-char wide display where the horizon line
    tilts with roll and shifts vertically with pitch.
    """

    DEFAULT_CSS = """
    AttitudeIndicator {
        height: 15;
        width: 30;
        padding: 0;
        margin: 0;
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
        super().__init__("", name=name, id=id, classes=classes)
        self._roll = 0.0
        self._pitch = 0.0
        self._render_horizon()

    def update_attitude(self, roll_deg: float, pitch_deg: float) -> None:
        """Update roll and pitch in degrees and re-render."""
        self._roll = roll_deg
        self._pitch = pitch_deg
        self._render_horizon()

    def _render_horizon(self) -> None:
        roll_rad = math.radians(max(-45, min(45, self._roll)))
        pitch_shift = max(-4, min(4, self._pitch / 10.0))

        mid_y = _HEIGHT // 2
        mid_x = _WIDTH // 2
        lines: list[str] = []

        for row in range(_HEIGHT):
            chars: list[str] = []
            for col in range(_WIDTH):
                dx = col - mid_x
                dy = row - mid_y
                # Horizon line position at this column
                horizon_y = mid_x * 0 + dx * math.tan(roll_rad) + pitch_shift
                rel = dy - horizon_y

                is_center = (row == mid_y and col == mid_x)
                is_crosshair = (
                    (row == mid_y and mid_x - 2 <= col <= mid_x + 2)
                    or (col == mid_x and mid_y - 1 <= row <= mid_y + 1)
                )

                if is_center:
                    chars.append("[#dff140]+[/#dff140]")
                elif is_crosshair:
                    if col == mid_x:
                        chars.append("[#dff140]|[/#dff140]")
                    else:
                        chars.append("[#dff140]-[/#dff140]")
                elif abs(rel) < 0.5:
                    chars.append("[#3a82ff]\u2550[/#3a82ff]")  # ═ horizon
                elif rel < 0:
                    chars.append("[#1a3a6a]\u2591[/#1a3a6a]")  # sky
                else:
                    chars.append("[#3a2a0a]\u2593[/#3a2a0a]")  # ground

            line_str = "".join(chars)
            # Border
            if row == 0 or row == _HEIGHT - 1:
                border = "[#1a1a2e]\u2500[/#1a1a2e]" * _WIDTH
                lines.append(border)
            else:
                lines.append(f"[#1a1a2e]\u2502[/#1a1a2e]{line_str}[#1a1a2e]\u2502[/#1a1a2e]")

        # Values below
        lines.append("")
        lines.append(
            f"  [#3a82ff]ROLL[/#3a82ff] {self._roll:+6.1f}\u00b0"
            f"  [#3a82ff]PITCH[/#3a82ff] {self._pitch:+6.1f}\u00b0"
        )

        self.update("\n".join(lines))
