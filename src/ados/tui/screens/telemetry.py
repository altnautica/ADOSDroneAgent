"""Telemetry screen — live attitude, GPS, battery, RC channels."""

from __future__ import annotations

import math

import httpx
import structlog
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import Sparkline, Static

log = structlog.get_logger("tui.telemetry")


class TelemetryScreen(Screen):
    """Live telemetry display."""

    def __init__(self) -> None:
        super().__init__()
        self._alt_history: list[float] = []
        self._speed_history: list[float] = []

    def compose(self) -> ComposeResult:
        with Horizontal():
            with Vertical(id="telem-left"):
                yield Static("[b]Attitude[/b]", classes="panel-title")
                yield Static("Loading...", id="attitude-panel")
                yield Static("[b]GPS[/b]", classes="panel-title")
                yield Static("Loading...", id="gps-panel")
                yield Static("[b]Battery[/b]", classes="panel-title")
                yield Static("Loading...", id="battery-panel")
            with Vertical(id="telem-right"):
                yield Static("[b]RC Channels[/b]", classes="panel-title")
                yield Static("Loading...", id="rc-panel")
                yield Static("[b]Altitude History[/b]", classes="panel-title")
                yield Sparkline([], id="alt-spark")
                yield Static("[b]Speed History[/b]", classes="panel-title")
                yield Sparkline([], id="speed-spark")

    def on_mount(self) -> None:
        self.set_interval(0.5, self._refresh)

    async def _refresh(self) -> None:
        api = self.app.api_url  # type: ignore[attr-defined]
        try:
            async with httpx.AsyncClient(timeout=3.0) as client:
                resp = await client.get(f"{api}/api/telemetry")
                data = resp.json()
        except httpx.ConnectError:
            self.query_one("#attitude-panel", Static).update(
                "Agent not running.\n\n"
                "Start with: ados demo    (simulated)\n"
                "       or:  ados start   (real FC)"
            )
            return
        except Exception as exc:
            log.warning("telemetry_refresh_failed", error=str(exc))
            self.query_one("#attitude-panel", Static).update("Error loading data")
            return

        # Attitude
        att = data.get("attitude", {})
        att_text = (
            f"Roll:  {math.degrees(att.get('roll', 0)):7.2f} deg\n"
            f"Pitch: {math.degrees(att.get('pitch', 0)):7.2f} deg\n"
            f"Yaw:   {math.degrees(att.get('yaw', 0)):7.2f} deg"
        )
        self.query_one("#attitude-panel", Static).update(att_text)

        # GPS
        pos = data.get("position", {})
        gps = data.get("gps", {})
        gps_text = (
            f"Lat:   {pos.get('lat', 0):.7f}\n"
            f"Lon:   {pos.get('lon', 0):.7f}\n"
            f"Alt:   {pos.get('alt_rel', 0):.1f} m (rel)\n"
            f"Speed: {data.get('velocity', {}).get('groundspeed', 0):.1f} m/s\n"
            f"Hdg:   {pos.get('heading', 0):.1f} deg\n"
            f"Fix:   {gps.get('fix_type', 0)}  Sats: {gps.get('satellites', 0)}"
        )
        self.query_one("#gps-panel", Static).update(gps_text)

        # Battery
        batt = data.get("battery", {})
        remaining = batt.get("remaining", -1)
        batt_text = (
            f"Voltage:   {batt.get('voltage', 0):.2f} V\n"
            f"Current:   {batt.get('current', 0):.2f} A\n"
            f"Remaining: {remaining}%\n"
            f"Cells:     {', '.join(f'{v:.2f}V' for v in batt.get('cell_voltages', []))}"
        )
        self.query_one("#battery-panel", Static).update(batt_text)

        # RC
        rc = data.get("rc", {})
        channels = rc.get("channels", [])
        rc_lines = []
        for i, val in enumerate(channels[:8], 1):
            bar = "#" * max(0, (val - 1000) // 25) if val > 0 else ""
            rc_lines.append(f"CH{i}: {val:4d} |{bar}")
        rc_lines.append(f"RSSI: {rc.get('rssi', 0)}")
        self.query_one("#rc-panel", Static).update("\n".join(rc_lines))

        # Sparklines
        alt = pos.get("alt_rel", 0)
        speed = data.get("velocity", {}).get("groundspeed", 0)
        self._alt_history.append(alt)
        self._speed_history.append(speed)
        if len(self._alt_history) > 120:
            self._alt_history = self._alt_history[-120:]
        if len(self._speed_history) > 120:
            self._speed_history = self._speed_history[-120:]

        self.query_one("#alt-spark", Sparkline).data = self._alt_history
        self.query_one("#speed-spark", Sparkline).data = self._speed_history
