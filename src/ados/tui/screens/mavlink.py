"""MAVLink inspector screen — message table with rates and filtering."""

from __future__ import annotations

import httpx
import structlog
from textual.app import ComposeResult
from textual.containers import Vertical
from textual.screen import Screen
from textual.widgets import DataTable, Input, Static

log = structlog.get_logger("tui.mavlink")


class MavlinkScreen(Screen):
    """MAVLink message inspector."""

    def __init__(self) -> None:
        super().__init__()
        self._msg_counts: dict[str, int] = {}
        self._msg_last: dict[str, str] = {}
        self._filter: str = ""

    def compose(self) -> ComposeResult:
        with Vertical():
            yield Static("[b]MAVLink Inspector[/b]", classes="panel-title")
            yield Input(placeholder="Filter by message name...", id="mav-filter")
            yield DataTable(id="mav-table")
            yield Static("", id="mav-stats")

    def on_mount(self) -> None:
        table = self.query_one("#mav-table", DataTable)
        table.add_columns("Message", "Count", "Last Value")
        self.set_interval(1.0, self._refresh)

    def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id == "mav-filter":
            self._filter = event.value.upper()

    async def _refresh(self) -> None:
        api = self.app.api_url  # type: ignore[attr-defined]
        try:
            async with httpx.AsyncClient(timeout=3.0) as client:
                resp = await client.get(f"{api}/api/telemetry")
                data = resp.json()
        except httpx.ConnectError:
            self.query_one("#mav-stats", Static).update(
                "Agent not running.\n"
                "Start with: ados demo    (simulated)\n"
                "       or:  ados start   (real FC)"
            )
            return
        except Exception as exc:
            log.warning("mavlink_refresh_failed", error=str(exc))
            self.query_one("#mav-stats", Static).update("Error loading data")
            return

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

        for name in messages:
            self._msg_counts[name] = self._msg_counts.get(name, 0) + 1
            self._msg_last[name] = messages[name]

        table = self.query_one("#mav-table", DataTable)
        table.clear()

        total_msgs = 0
        for name, count in sorted(self._msg_counts.items()):
            if self._filter and self._filter not in name:
                continue
            total_msgs += count
            table.add_row(name, str(count), self._msg_last.get(name, ""))

        stats = f"Messages: {len(self._msg_counts)} types, {total_msgs} total"
        self.query_one("#mav-stats", Static).update(stats)
