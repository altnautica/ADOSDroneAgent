"""Telemetry screen — attitude indicator, sparklines, battery, RC, GPS."""

from __future__ import annotations

import math

import structlog
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import Static

from ados.tui.widgets import (
    AttitudeIndicator,
    GaugeBar,
    InfoPanel,
    SatelliteBar,
    SparklinePanel,
)

log = structlog.get_logger("tui.telemetry")


class TelemetryScreen(Screen):
    """Live telemetry display with attitude indicator, sparklines, and gauges."""

    DEFAULT_CSS = """
    TelemetryScreen {
        layout: vertical;
    }
    #telem-top {
        height: 18;
    }
    #telem-top-left {
        width: 35;
    }
    #telem-top-right {
        width: 1fr;
    }
    #telem-bottom {
        height: 1fr;
    }
    .telem-col {
        width: 1fr;
    }
    """

    def compose(self) -> ComposeResult:
        # Top row: attitude indicator + sparklines
        with Horizontal(id="telem-top"):
            with Vertical(id="telem-top-left"):
                with InfoPanel("ATTITUDE"):
                    yield AttitudeIndicator(id="attitude")

            with Vertical(id="telem-top-right"):
                yield SparklinePanel("ALTITUDE", maxlen=60, unit="m", id="spark-alt")
                yield SparklinePanel("SPEED", maxlen=60, unit="m/s", id="spark-speed")
                yield SparklinePanel("CLIMB RATE", maxlen=60, unit="m/s", id="spark-climb")

        # Bottom row: battery, RC channels, GPS
        with Horizontal(id="telem-bottom"):
            # Battery
            with InfoPanel("BATTERY", classes="telem-col"):
                yield GaugeBar("Total", id="telem-batt-gauge", thresholds=(999.0, 999.0))
                yield Static("", id="telem-batt-info")
                yield Vertical(id="telem-cell-gauges")

            # RC Channels
            with InfoPanel("RC CHANNELS", classes="telem-col"):
                yield GaugeBar("CH1", id="rc-ch1", thresholds=(999.0, 999.0), suffix="")
                yield GaugeBar("CH2", id="rc-ch2", thresholds=(999.0, 999.0), suffix="")
                yield GaugeBar("CH3", id="rc-ch3", thresholds=(999.0, 999.0), suffix="")
                yield GaugeBar("CH4", id="rc-ch4", thresholds=(999.0, 999.0), suffix="")
                yield GaugeBar("CH5", id="rc-ch5", thresholds=(999.0, 999.0), suffix="")
                yield GaugeBar("CH6", id="rc-ch6", thresholds=(999.0, 999.0), suffix="")
                yield GaugeBar("CH7", id="rc-ch7", thresholds=(999.0, 999.0), suffix="")
                yield GaugeBar("CH8", id="rc-ch8", thresholds=(999.0, 999.0), suffix="")

            # GPS
            with InfoPanel("GPS", classes="telem-col"):
                yield Static("", id="telem-gps-info")
                yield SatelliteBar(max_sats=20, id="telem-sat-bar")

    def on_mount(self) -> None:
        self.set_interval(0.5, self._refresh)

    async def _refresh(self) -> None:
        fetcher = self.app.fetcher  # type: ignore[attr-defined]
        data = await fetcher.get_telemetry()

        if data is None:
            try:
                self.query_one("#attitude", AttitudeIndicator).update(
                    "[#ef4444]Agent not running[/#ef4444]\n\n"
                    "Start with: ados demo   (simulated)\n"
                    "       or:  ados start  (real FC)"
                )
            except Exception:
                pass
            return

        # -- Attitude --
        att = data.get("attitude", {})
        roll_deg = math.degrees(att.get("roll", 0))
        pitch_deg = math.degrees(att.get("pitch", 0))
        try:
            self.query_one("#attitude", AttitudeIndicator).update_attitude(roll_deg, pitch_deg)
        except Exception:
            pass

        # -- Sparklines --
        pos = data.get("position", {})
        vel = data.get("velocity", {})
        alt = pos.get("alt_rel", 0)
        speed = vel.get("groundspeed", 0)
        climb = vel.get("climb", 0) if "climb" in vel else pos.get("vz", 0)

        fetcher.push_sample("altitude", alt)
        fetcher.push_sample("speed", speed)
        fetcher.push_sample("climb", climb)

        try:
            self.query_one("#spark-alt", SparklinePanel).push(alt)
        except Exception:
            pass
        try:
            self.query_one("#spark-speed", SparklinePanel).push(speed)
        except Exception:
            pass
        try:
            self.query_one("#spark-climb", SparklinePanel).push(climb)
        except Exception:
            pass

        # -- Battery --
        batt = data.get("battery", {})
        remaining = batt.get("remaining", -1)
        voltage = batt.get("voltage", 0)
        current = batt.get("current", 0)
        cells = batt.get("cell_voltages", [])

        batt_pct = max(0, remaining) if remaining >= 0 else 0
        try:
            gauge = self.query_one("#telem-batt-gauge", GaugeBar)
            # Invert: low = bad
            if batt_pct > 50:
                gauge._thresholds = (999.0, 999.0)
            elif batt_pct > 20:
                gauge._thresholds = (0.0, 999.0)
            else:
                gauge._thresholds = (0.0, 0.0)
            gauge.update_value(batt_pct)
        except Exception:
            pass

        batt_text = (
            f"[#fafafa]{voltage:.2f}V[/#fafafa] / "
            f"[#fafafa]{current:.1f}A[/#fafafa]  "
            f"[#666666]{remaining}%[/#666666]"
        )
        try:
            self.query_one("#telem-batt-info", Static).update(batt_text)
        except Exception:
            pass

        # Cell voltage gauges (dynamic)
        if cells:
            container = self.query_one("#telem-cell-gauges", Vertical)
            # Only rebuild if cell count changed
            current_count = len(list(container.children))
            if current_count != len(cells):
                for child in list(container.children):
                    child.remove()
                for i, _v in enumerate(cells):
                    container.mount(
                        GaugeBar(f"C{i+1}", id=f"cell-{i}", thresholds=(3.5, 3.3), suffix="V")
                    )
            # Update values: map voltage 3.0-4.2 to 0-100
            for i, v in enumerate(cells):
                try:
                    pct = max(0, min(100, (v - 3.0) / 1.2 * 100))
                    cell_gauge = self.query_one(f"#cell-{i}", GaugeBar)
                    # For cells: low voltage = bad
                    if pct > 40:
                        cell_gauge._thresholds = (999.0, 999.0)
                    elif pct > 15:
                        cell_gauge._thresholds = (0.0, 999.0)
                    else:
                        cell_gauge._thresholds = (0.0, 0.0)
                    cell_gauge.update_value(pct)
                except Exception:
                    pass

        # -- RC Channels --
        rc = data.get("rc", {})
        channels = rc.get("channels", [])
        for i, val in enumerate(channels[:8]):
            try:
                # Map 1000-2000 to 0-100
                pct = max(0, min(100, (val - 1000) / 10.0))
                gauge = self.query_one(f"#rc-ch{i+1}", GaugeBar)
                gauge._suffix = f" {val}"
                # Center stick channels (1-4): mid=green, edges=yellow
                if i < 4:
                    mid_dev = abs(val - 1500)
                    if mid_dev < 100:
                        gauge._thresholds = (999.0, 999.0)
                    elif mid_dev < 300:
                        gauge._thresholds = (0.0, 999.0)
                    else:
                        gauge._thresholds = (0.0, 0.0)
                else:
                    gauge._thresholds = (999.0, 999.0)
                gauge.update_value(pct)
            except Exception:
                pass

        # -- GPS --
        gps = data.get("gps", {})
        lat = pos.get("lat", 0)
        lon = pos.get("lon", 0)
        heading = pos.get("heading", 0)
        fix = gps.get("fix_type", 0)
        sats = gps.get("satellites", 0)
        hdop = gps.get("hdop", 0)
        vz = vel.get("climb", 0) if "climb" in vel else 0

        fix_labels = {
            0: "No GPS", 1: "No Fix", 2: "2D", 3: "3D",
            4: "DGPS", 5: "RTK Float", 6: "RTK Fix",
        }
        fix_label = fix_labels.get(fix, f"Type {fix}")

        gps_text = (
            f"Lat:  [#fafafa]{lat:.7f}[/#fafafa]\n"
            f"Lon:  [#fafafa]{lon:.7f}[/#fafafa]\n"
            f"Alt:  [#dff140]{alt:.1f}m[/#dff140]\n"
            f"Hdg:  [#3a82ff]{heading:.1f}\u00b0[/#3a82ff]\n"
            f"Fix:  [#3a82ff]{fix_label}[/#3a82ff]\n"
            f"HDOP: [#666666]{hdop:.1f}[/#666666]\n"
            f"Vz:   [#fafafa]{vz:.1f} m/s[/#fafafa]"
        )
        try:
            self.query_one("#telem-gps-info", Static).update(gps_text)
        except Exception:
            pass
        try:
            self.query_one("#telem-sat-bar", SatelliteBar).update_count(sats)
        except Exception:
            pass
