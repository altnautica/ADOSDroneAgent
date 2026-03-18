"""Dashboard screen — system health, FC info, GPS, link, services, events."""

from __future__ import annotations

import structlog
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import RichLog, Static

from ados.tui.widgets import (
    GaugeBar,
    InfoPanel,
    SatelliteBar,
    StatusDot,
)

log = structlog.get_logger("tui.dashboard")


def _fmt_uptime(seconds: int) -> str:
    """Format seconds into human-readable uptime."""
    if seconds < 60:
        return f"{seconds}s"
    if seconds < 3600:
        return f"{seconds // 60}m {seconds % 60}s"
    hours = seconds // 3600
    mins = (seconds % 3600) // 60
    return f"{hours}h {mins}m"


class DashboardScreen(Screen):
    """Main dashboard with system health, FC, GPS, link, services, events."""

    DEFAULT_CSS = """
    DashboardScreen {
        layout: vertical;
    }
    #dash-banner {
        height: 3;
        padding: 0 1;
        background: #0a0a0f;
        border-bottom: solid #1a1a2e;
    }
    #dash-banner-row {
        height: 3;
    }
    #dash-grid {
        height: 1fr;
        background: #0a0a0f;
    }
    .dash-row {
        height: 1fr;
    }
    .dash-col {
        width: 1fr;
        height: 1fr;
    }
    #dash-events {
        height: auto;
        max-height: 12;
        margin: 0 1;
        background: #0a0a0f;
    }
    """

    def compose(self) -> ComposeResult:
        # Top banner: agent info + pairing
        with Vertical(id="dash-banner"):
            with Horizontal(id="dash-banner-row"):
                yield Static("", id="banner-info")
                yield StatusDot("Pairing", "unknown", id="banner-pairing")
                yield StatusDot("FC", "unknown", id="banner-fc")

        # Middle grid: 3 columns, 2 rows
        with Vertical(id="dash-grid"):
            with Horizontal(classes="dash-row"):
                # System
                with InfoPanel("SYSTEM", id="panel-system", classes="dash-col"):
                    yield GaugeBar("CPU", id="gauge-cpu")
                    yield GaugeBar("RAM", id="gauge-ram")
                    yield GaugeBar("Disk", id="gauge-disk")
                    yield GaugeBar(
                        "Temp", id="gauge-temp",
                        thresholds=(65.0, 80.0), suffix="\u00b0C",
                    )

                # Flight Controller
                with InfoPanel("FLIGHT CONTROLLER", id="panel-fc", classes="dash-col"):
                    yield StatusDot("Armed", "unknown", id="dot-armed")
                    yield Static("", id="fc-details")

                # Battery
                with InfoPanel("BATTERY", id="panel-battery", classes="dash-col"):
                    yield GaugeBar("Batt", id="gauge-batt", thresholds=(30.0, 60.0))
                    yield Static("", id="batt-details")

            with Horizontal(classes="dash-row"):
                # GPS
                with InfoPanel("GPS", id="panel-gps", classes="dash-col"):
                    yield Static("", id="gps-info")
                    yield SatelliteBar(max_sats=20, id="sat-bar")

                # Link
                with InfoPanel("LINK", id="panel-link", classes="dash-col"):
                    yield GaugeBar("RSSI", id="gauge-rssi", thresholds=(40.0, 70.0))
                    yield Static("", id="link-info")

                # Services
                with InfoPanel("SERVICES", id="panel-services", classes="dash-col"):
                    yield Vertical(id="services-list")

        # Bottom: events log
        with InfoPanel("EVENTS", id="dash-events"):
            yield RichLog(id="events-log", max_lines=8, markup=True)

    def on_mount(self) -> None:
        self.set_interval(1.0, self._refresh)

    async def _refresh(self) -> None:
        fetcher = self.app.fetcher  # type: ignore[attr-defined]

        status = await fetcher.get_status()
        services = await fetcher.get_services()
        logs = await fetcher.get_logs(limit=8)
        pairing = await fetcher.get_pairing()
        telemetry = await fetcher.get_telemetry()

        if status is None:
            try:
                self.query_one("#banner-info", Static).update(
                    "[#ef4444]Agent not running[/#ef4444]"
                )
            except Exception:
                pass
            return

        # -- Banner --
        board = status.get("board", {})
        uptime = status.get("uptime_seconds", 0)
        version = status.get("version", "?")
        board_name = board.get("name", "?")
        tier = board.get("tier", "?")

        banner_text = (
            f"[#3a82ff]v{version}[/#3a82ff]  "
            f"Board: [#fafafa]{board_name}[/#fafafa]  "
            f"Tier: [#dff140]{tier}[/#dff140]  "
            f"Up: [#666666]{_fmt_uptime(int(uptime))}[/#666666]"
        )
        try:
            self.query_one("#banner-info", Static).update(banner_text)
        except Exception:
            pass

        # Pairing
        if pairing:
            if pairing.get("paired"):
                try:
                    self.query_one("#banner-pairing", StatusDot).set_state("paired")
                except Exception:
                    pass
            else:
                try:
                    dot = self.query_one("#banner-pairing", StatusDot)
                    dot.set_state("unpaired")
                except Exception:
                    pass

        # FC connection
        fc_connected = status.get("fc_connected", False)
        try:
            self.query_one("#banner-fc", StatusDot).set_state(
                "connected" if fc_connected else "disconnected"
            )
        except Exception:
            pass

        # -- System gauges --
        health = status.get("health", {})
        cpu = health.get("cpu_percent", 0)
        ram = health.get("memory_percent", 0)
        disk = health.get("disk_percent", 0)
        temp = health.get("temperature", 0)
        if temp is None:
            temp = 0

        fetcher.push_sample("cpu", cpu)
        fetcher.push_sample("ram", ram)

        try:
            self.query_one("#gauge-cpu", GaugeBar).update_value(cpu)
        except Exception:
            pass
        try:
            self.query_one("#gauge-ram", GaugeBar).update_value(ram)
        except Exception:
            pass
        try:
            self.query_one("#gauge-disk", GaugeBar).update_value(disk)
        except Exception:
            pass
        try:
            # Temp gauge: map 0-100C to 0-100%
            temp_val = float(temp) if temp else 0
            self.query_one("#gauge-temp", GaugeBar).update_value(temp_val)
        except Exception:
            pass

        # -- Flight Controller --
        fc_port = status.get("fc_port", "N/A")
        fc_baud = status.get("fc_baud", "N/A")
        mode = "N/A"
        armed = False

        if telemetry:
            mode = telemetry.get("mode", "N/A")
            armed = telemetry.get("armed", False)

        try:
            self.query_one("#dot-armed", StatusDot).set_state(
                "armed" if armed else "idle"
            )
        except Exception:
            pass

        fc_text = (
            f"Mode: [#fafafa]{mode}[/#fafafa]\n"
            f"Port: [#666666]{fc_port}[/#666666]\n"
            f"Baud: [#666666]{fc_baud}[/#666666]"
        )
        try:
            self.query_one("#fc-details", Static).update(fc_text)
        except Exception:
            pass

        # -- Battery --
        if telemetry:
            batt = telemetry.get("battery", {})
            remaining = batt.get("remaining", -1)
            voltage = batt.get("voltage", 0)
            current = batt.get("current", 0)
            cells = batt.get("cell_voltages", [])

            batt_pct = max(0, remaining) if remaining >= 0 else 0
            # Invert thresholds: low battery = bad
            try:
                gauge = self.query_one("#gauge-batt", GaugeBar)
                gauge._thresholds = (999.0, 999.0)  # Override: use custom logic
                if batt_pct > 50:
                    gauge._thresholds = (999.0, 999.0)
                elif batt_pct > 20:
                    gauge._thresholds = (0.0, 999.0)
                else:
                    gauge._thresholds = (0.0, 0.0)
                gauge.update_value(batt_pct)
            except Exception:
                pass

            cell_str = " ".join(f"{v:.2f}V" for v in cells) if cells else "N/A"
            batt_text = (
                f"[#fafafa]{voltage:.2f}V[/#fafafa] / "
                f"[#fafafa]{current:.1f}A[/#fafafa]\n"
                f"Cells: [#666666]{cell_str}[/#666666]"
            )
            try:
                self.query_one("#batt-details", Static).update(batt_text)
            except Exception:
                pass

        # -- GPS --
        if telemetry:
            pos = telemetry.get("position", {})
            gps = telemetry.get("gps", {})
            lat = pos.get("lat", 0)
            lon = pos.get("lon", 0)
            alt = pos.get("alt_rel", 0)
            fix = gps.get("fix_type", 0)
            sats = gps.get("satellites", 0)

            fix_labels = {
                0: "No GPS", 1: "No Fix", 2: "2D", 3: "3D",
                4: "DGPS", 5: "RTK Float", 6: "RTK Fix",
            }
            fix_label = fix_labels.get(fix, f"Type {fix}")

            gps_text = (
                f"Lat: [#fafafa]{lat:.7f}[/#fafafa]\n"
                f"Lon: [#fafafa]{lon:.7f}[/#fafafa]\n"
                f"Alt: [#dff140]{alt:.1f}m[/#dff140]  "
                f"Fix: [#3a82ff]{fix_label}[/#3a82ff]"
            )
            try:
                self.query_one("#gps-info", Static).update(gps_text)
            except Exception:
                pass
            try:
                self.query_one("#sat-bar", SatelliteBar).update_count(sats)
            except Exception:
                pass

        # -- Link --
        wfb = await fetcher.get_wfb()
        if wfb:
            rssi = wfb.get("rssi_dbm", -100)
            snr = wfb.get("snr_db", 0)
            loss = wfb.get("loss_percent", 0)
            bitrate = wfb.get("bitrate_kbps", 0)
            state = wfb.get("state", "disabled")

            # Map RSSI -100..-30 to 0..100
            rssi_pct = max(0, min(100, (rssi + 100) / 70 * 100))
            fetcher.push_sample("rssi", float(rssi))

            try:
                self.query_one("#gauge-rssi", GaugeBar).update_value(rssi_pct)
            except Exception:
                pass

            if state == "connected":
                state_color = "#22c55e"
            elif state == "connecting":
                state_color = "#f59e0b"
            else:
                state_color = "#ef4444"
            link_text = (
                f"[{state_color}]{state.upper()}[/{state_color}]  "
                f"SNR: [#fafafa]{snr:.1f}dB[/#fafafa]  "
                f"Loss: [#fafafa]{loss:.1f}%[/#fafafa]\n"
                f"Bitrate: [#3a82ff]{bitrate}kbps[/#3a82ff]"
            )
            try:
                self.query_one("#link-info", Static).update(link_text)
            except Exception:
                pass

        # -- Services --
        if services:
            svc_list = services.get("services", [])
            container = self.query_one("#services-list", Vertical)

            # Remove old dots and re-add
            try:
                for child in list(container.children):
                    child.remove()
            except Exception:
                pass

            for svc in svc_list:
                svc_name = svc.get("name", "?")
                svc_status = svc.get("status", "unknown")
                dot = StatusDot(svc_name, svc_status)
                container.mount(dot)

        # -- Events log --
        if logs:
            entries = logs.get("entries", [])
            try:
                rich_log = self.query_one("#events-log", RichLog)
                rich_log.clear()
                for e in entries[-8:]:
                    level = e.get("level", "INFO")
                    msg = e.get("message", "")[:80]
                    ts = e.get("timestamp", "")
                    if ts:
                        ts = ts[-8:]  # HH:MM:SS
                    if level in ("ERROR", "CRITICAL"):
                        color = "#ef4444"
                    elif level == "WARNING":
                        color = "#f59e0b"
                    else:
                        color = "#666666"
                    rich_log.write(f"[{color}]{ts} [{level}][/{color}] {msg}")
            except Exception:
                pass

        # -- Update status bar --
        try:
            from ados.tui.widgets import AgentStatusBar
            bar = self.app.query_one("#agent-status-bar", AgentStatusBar)
            fc_state_str = mode if fc_connected else "disconnected"
            rssi_val = int(wfb.get("rssi_dbm", -100)) if wfb else -100
            batt_val = int(telemetry.get("battery", {}).get("remaining", -1)) if telemetry else -1
            bar.update_status(
                fc_state=fc_state_str,
                rssi=rssi_val,
                battery=batt_val,
                uptime=int(uptime),
            )
        except Exception:
            pass
