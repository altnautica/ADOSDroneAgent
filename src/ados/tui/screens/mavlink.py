"""MAVLink inspector screen — message table with rates and filtering."""

from __future__ import annotations

import structlog
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import DataTable, Input, Static

from ados.tui.widgets import InfoPanel, StatusDot

log = structlog.get_logger("tui.mavlink")

# Approximate rates (Hz) for common MAVLink messages in ArduPilot defaults
_DEFAULT_RATES: dict[str, float] = {
    "HEARTBEAT": 1.0,
    "GLOBAL_POSITION_INT": 5.0,
    "ATTITUDE": 10.0,
    "SYS_STATUS": 2.0,
    "GPS_RAW_INT": 5.0,
    "VFR_HUD": 4.0,
    "BATTERY_STATUS": 2.0,
    "RC_CHANNELS": 4.0,
}


class MavlinkScreen(Screen):
    """MAVLink message inspector."""

    def __init__(self) -> None:
        super().__init__()
        self._msg_counts: dict[str, int] = {}
        self._msg_last: dict[str, str] = {}
        self._filter: str = ""
        self._tick: int = 0

    def compose(self) -> ComposeResult:
        with Vertical():
            with InfoPanel("MAVLINK INSPECTOR"):
                with Horizontal():
                    yield StatusDot("Stream", "disconnected", id="mav-stream-dot")
                    yield Static("  ", classes="spacer")
                    yield Static("0 msg/s", id="mav-rate")
                    yield Static("  ", classes="spacer")
                    yield Static("0 total", id="mav-total")
                yield Static("signing: --", id="mav-signing-status")
                yield Input(placeholder="Filter by message name...", id="mav-filter")
            with InfoPanel("MESSAGES"):
                yield DataTable(id="mav-table")

    def on_mount(self) -> None:
        table = self.query_one("#mav-table", DataTable)
        table.add_columns("Message", "Count", "Hz", "Last Value")
        self.set_interval(1.0, self._refresh)

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id == "mav-filter":
            self._filter = event.value.upper()

    async def _refresh(self) -> None:
        fetcher = self.app.fetcher  # type: ignore[attr-defined]
        data = await fetcher.get_telemetry()

        stream_dot = self.query_one("#mav-stream-dot", StatusDot)

        if data is None:
            stream_dot.set_state("disconnected")
            self.query_one("#mav-rate", Static).update("-- msg/s")
            return

        stream_dot.set_state("connected")

        # Build pseudo message table from telemetry
        pos = data.get("position", {})
        att = data.get("attitude", {})
        batt = data.get("battery", {})
        gps = data.get("gps", {})
        vel = data.get("velocity", {})
        rc = data.get("rc", {})
        ch = rc.get("channels", [0])
        messages = {
            "HEARTBEAT": f"armed={data.get('armed', '?')}, mode={data.get('mode', '?')}",
            "GLOBAL_POSITION_INT": f"lat={pos.get('lat', 0):.5f}, lon={pos.get('lon', 0):.5f}",
            "ATTITUDE": f"roll={att.get('roll', 0):.2f}, pitch={att.get('pitch', 0):.2f}",
            "SYS_STATUS": f"batt={batt.get('voltage', 0):.1f}V",
            "GPS_RAW_INT": f"fix={gps.get('fix_type', 0)}, sats={gps.get('satellites', 0)}",
            "VFR_HUD": f"gs={vel.get('groundspeed', 0):.1f}, throttle={data.get('throttle', 0)}",
            "BATTERY_STATUS": f"remaining={batt.get('remaining', -1)}%",
            "RC_CHANNELS": f"ch1={ch[0] if ch else 0}",
        }

        prev_total = sum(self._msg_counts.values())

        for name in messages:
            self._msg_counts[name] = self._msg_counts.get(name, 0) + 1
            self._msg_last[name] = messages[name]

        new_total = sum(self._msg_counts.values())
        rate = new_total - prev_total

        self._tick += 1

        # Update header stats
        self.query_one("#mav-rate", Static).update(f"{rate} msg/s")
        self.query_one("#mav-total", Static).update(f"{new_total} total")

        # Signing status line. Keys live in the GCS browser; the agent only
        # reports capability and observed signed-frame counters.
        signing_line = self.query_one("#mav-signing-status", Static)
        try:
            cap = await fetcher.get_signing_capability()
            ctr = await fetcher.get_signing_counters()
        except Exception:
            cap, ctr = None, None
        if cap is None:
            signing_line.update("signing: unknown")
        elif cap.get("supported"):
            tx = ctr.get("tx_signed_count", 0) if ctr else 0
            rx = ctr.get("rx_signed_count", 0) if ctr else 0
            fw = cap.get("firmware_name") or "ArduPilot"
            signing_line.update(
                f"signing: supported ({fw})   signed frames tx={tx} rx={rx}"
            )
        else:
            reason = cap.get("reason") or "unknown"
            signing_line.update(f"signing: not available ({reason})")

        # Rebuild table
        table = self.query_one("#mav-table", DataTable)
        table.clear()

        for name, count in sorted(self._msg_counts.items()):
            if self._filter and self._filter not in name:
                continue
            hz = _DEFAULT_RATES.get(name, 0.0)
            # Color-code by rate: >10 Hz = lime, >1 Hz = blue, else dim
            if hz > 10:
                styled_name = f"[#dff140]{name}[/#dff140]"
            elif hz > 1:
                styled_name = f"[#3a82ff]{name}[/#3a82ff]"
            else:
                styled_name = f"[#666666]{name}[/#666666]"
            table.add_row(styled_name, str(count), f"{hz:.0f}", self._msg_last.get(name, ""))
