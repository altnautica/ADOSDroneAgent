"""AgentStatusBar widget — bottom bar with version, FC, RSSI, battery, uptime."""

from __future__ import annotations

from textual.widgets import Static


class AgentStatusBar(Static):
    """Bottom status bar showing agent state at a glance.

    Displays: version | FC state | RSSI | battery % | uptime | hotkey hints
    """

    DEFAULT_CSS = """
    AgentStatusBar {
        dock: bottom;
        height: 1;
        background: #0a0a0f;
        color: #666666;
        padding: 0 1;
    }
    """

    def __init__(
        self,
        version: str = "",
        *,
        name: str | None = None,
        id: str | None = None,
        classes: str | None = None,
    ) -> None:
        super().__init__("", name=name, id=id, classes=classes)
        self._version = version
        self._fc_state = "N/A"
        self._rssi = -100
        self._battery = -1
        self._uptime = 0
        self._render_bar()

    def update_status(
        self,
        fc_state: str = "N/A",
        rssi: int = -100,
        battery: int = -1,
        uptime: int = 0,
    ) -> None:
        """Update status values and re-render."""
        self._fc_state = fc_state
        self._rssi = rssi
        self._battery = battery
        self._uptime = uptime
        self._render_bar()

    def _render_bar(self) -> None:
        ver = f"[#3a82ff]ADOS[/#3a82ff] {self._version}" if self._version else "[#3a82ff]ADOS[/#3a82ff]"

        # FC state
        fc_color = "#22c55e" if self._fc_state not in ("N/A", "disconnected") else "#666666"
        fc = f"[{fc_color}]FC:{self._fc_state}[/{fc_color}]"

        # RSSI
        if self._rssi > -60:
            rssi_c = "#22c55e"
        elif self._rssi > -75:
            rssi_c = "#f59e0b"
        else:
            rssi_c = "#ef4444"
        rssi_str = f"[{rssi_c}]RSSI:{self._rssi}dBm[/{rssi_c}]"

        # Battery
        if self._battery < 0:
            batt = "[#666666]BAT:--[/#666666]"
        elif self._battery > 50:
            batt = f"[#22c55e]BAT:{self._battery}%[/#22c55e]"
        elif self._battery > 20:
            batt = f"[#f59e0b]BAT:{self._battery}%[/#f59e0b]"
        else:
            batt = f"[#ef4444]BAT:{self._battery}%[/#ef4444]"

        # Uptime
        mins = self._uptime // 60
        secs = self._uptime % 60
        up = f"[#666666]UP:{mins}m{secs:02d}s[/#666666]"

        # Hotkeys
        keys = "[#666666]d:Dash t:Telem w:Link q:Quit[/#666666]"

        self.update(f" {ver}  {fc}  {rssi_str}  {batt}  {up}  {keys}")
