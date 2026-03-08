"""OSD overlay — generates ffmpeg drawtext filter strings from telemetry data."""

from __future__ import annotations

from typing import TYPE_CHECKING

from ados.core.logging import get_logger

if TYPE_CHECKING:
    from ados.services.mavlink.state import VehicleState

log = get_logger("video.osd")

_FONT_SIZE = 16
_FONT_COLOR = "white"
_BOX_COLOR = "black@0.5"
_FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"


def _escape_drawtext(text: str) -> str:
    """Escape special characters for ffmpeg drawtext filter."""
    return text.replace("\\", "\\\\").replace(":", "\\:").replace("'", "\\'")


def build_osd_filter(state: VehicleState) -> str:
    """Build an ffmpeg drawtext filter string from current vehicle state.

    The OSD displays altitude, speed, battery, GPS, mode, and armed status
    in the top-left corner of the video frame.

    Args:
        state: Current vehicle state with telemetry data.

    Returns:
        An ffmpeg ``-vf`` compatible filter string.
    """
    armed_str = "ARMED" if state.armed else "DISARMED"
    mode_str = state.mode or "UNKNOWN"

    lines = [
        f"ALT: {state.alt_rel:.1f}m",
        f"SPD: {state.groundspeed:.1f}m/s",
        f"BAT: {state.voltage_battery:.1f}V ({state.battery_remaining}%)",
        f"GPS: {state.lat:.6f}, {state.lon:.6f}",
        f"SAT: {state.gps_satellites} FIX: {state.gps_fix_type}",
        f"HDG: {state.heading:.0f} deg",
        f"{mode_str} | {armed_str}",
    ]

    filters: list[str] = []
    for idx, line in enumerate(lines):
        escaped = _escape_drawtext(line)
        y_offset = 10 + idx * (_FONT_SIZE + 6)
        dt = (
            f"drawtext=text='{escaped}'"
            f":fontfile={_FONT}"
            f":fontsize={_FONT_SIZE}"
            f":fontcolor={_FONT_COLOR}"
            f":box=1:boxcolor={_BOX_COLOR}:boxborderw=4"
            f":x=10:y={y_offset}"
        )
        filters.append(dt)

    result = ",".join(filters)
    log.debug("osd_filter_built", lines=len(lines))
    return result


def build_osd_command_args(state: VehicleState) -> list[str]:
    """Return ffmpeg arguments to apply the OSD filter.

    Usage: insert these args into an ffmpeg command before the output file.
    """
    vf = build_osd_filter(state)
    return ["-vf", vf]
