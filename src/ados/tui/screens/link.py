"""Link screen — WFB-ng RSSI, packet loss, FEC stats, channel info."""

from __future__ import annotations

import httpx
import structlog
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import Static

log = structlog.get_logger("tui.link")

# RSSI thresholds for color coding
_RSSI_EXCELLENT = -50
_RSSI_GOOD = -60
_RSSI_FAIR = -70
_RSSI_POOR = -80


def _rssi_bar(rssi: float, width: int = 20) -> str:
    """Generate a colored RSSI bar string for the TUI.

    Maps RSSI from -100 (empty) to -30 (full) onto a bar of given width.
    Uses Textual rich markup for color.
    """
    # Clamp RSSI to displayable range
    clamped = max(-100.0, min(-30.0, rssi))
    fill = int((clamped + 100) / 70.0 * width)
    fill = max(0, min(width, fill))

    if rssi >= _RSSI_EXCELLENT:
        color = "green"
    elif rssi >= _RSSI_GOOD:
        color = "cyan"
    elif rssi >= _RSSI_FAIR:
        color = "yellow"
    elif rssi >= _RSSI_POOR:
        color = "dark_orange"
    else:
        color = "red"

    filled = "\u2588" * fill
    empty = "\u2591" * (width - fill)
    return f"[{color}]{filled}[/{color}]{empty}"


def _state_color(state: str) -> str:
    """Get a color name for the link state."""
    colors = {
        "connected": "green",
        "connecting": "yellow",
        "degraded": "dark_orange",
        "disconnected": "red",
        "disabled": "dim",
    }
    return colors.get(state, "white")


class LinkScreen(Screen):
    """WFB-ng video link status display.

    Polls the /api/wfb endpoint every second and displays:
    - Link state with color indicator
    - RSSI bar graph
    - Packet loss percentage
    - FEC recovery stats
    - Channel information
    - Bitrate
    """

    def compose(self) -> ComposeResult:
        with Vertical():
            yield Static("[b]WFB-ng Video Link[/b]", classes="panel-title")
            with Horizontal():
                with Vertical(id="link-left"):
                    yield Static("Loading...", id="link-state")
                    yield Static("", id="rssi-display")
                    yield Static("", id="packet-stats")
                with Vertical(id="link-right"):
                    yield Static("", id="fec-stats")
                    yield Static("", id="channel-info")
                    yield Static("", id="link-meta")

    def on_mount(self) -> None:
        self.set_interval(1.0, self._refresh)

    async def _refresh(self) -> None:
        api = self.app.api_url  # type: ignore[attr-defined]
        try:
            async with httpx.AsyncClient(timeout=3.0) as client:
                resp = await client.get(f"{api}/api/wfb")
                data = resp.json()
        except httpx.ConnectError:
            self.query_one("#link-state", Static).update(
                "[red]Agent not running[/red]"
            )
            return
        except Exception as exc:
            log.warning("link_refresh_failed", error=str(exc))
            self.query_one("#link-state", Static).update(
                "[red]Error loading data[/red]"
            )
            return

        state = data.get("state", "disabled")
        color = _state_color(state)
        rssi = data.get("rssi_dbm", -100.0)
        noise = data.get("noise_dbm", -95.0)
        snr = data.get("snr_db", 0.0)
        loss = data.get("loss_percent", 0.0)
        pkts_rx = data.get("packets_received", 0)
        pkts_lost = data.get("packets_lost", 0)
        fec_rec = data.get("fec_recovered", 0)
        fec_fail = data.get("fec_failed", 0)
        bitrate = data.get("bitrate_kbps", 0)
        channel = data.get("channel", 0)
        interface = data.get("interface", "N/A")
        samples = data.get("samples", 0)
        restarts = data.get("restart_count", 0)

        # Link state
        state_text = f"State: [{color}]{state.upper()}[/{color}]"
        self.query_one("#link-state", Static).update(state_text)

        # RSSI with bar
        bar = _rssi_bar(rssi)
        rssi_text = (
            f"RSSI:  {rssi:>6.1f} dBm  {bar}\n"
            f"Noise: {noise:>6.1f} dBm\n"
            f"SNR:   {snr:>6.1f} dB"
        )
        self.query_one("#rssi-display", Static).update(rssi_text)

        # Packet stats
        loss_color = "green" if loss < 1.0 else ("yellow" if loss < 5.0 else "red")
        pkt_text = (
            f"Packets RX: {pkts_rx:>8}\n"
            f"Packets Lost: {pkts_lost:>6}\n"
            f"Loss: [{loss_color}]{loss:>5.1f}%[/{loss_color}]"
        )
        self.query_one("#packet-stats", Static).update(pkt_text)

        # FEC stats
        fec_text = (
            f"FEC Recovered: {fec_rec}\n"
            f"FEC Failed:    {fec_fail}\n"
            f"Bitrate: {bitrate} kbps"
        )
        self.query_one("#fec-stats", Static).update(fec_text)

        # Channel info
        ch_text = (
            f"Channel:   {channel}\n"
            f"Interface: {interface}"
        )
        self.query_one("#channel-info", Static).update(ch_text)

        # Meta
        meta_text = (
            f"Samples: {samples}\n"
            f"Restarts: {restarts}"
        )
        self.query_one("#link-meta", Static).update(meta_text)
