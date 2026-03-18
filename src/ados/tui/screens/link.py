"""Link screen — WFB-ng RSSI, signal quality, throughput, packets, FEC, channel."""

from __future__ import annotations

import structlog
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import Static

from ados.tui.widgets import (
    GaugeBar,
    InfoPanel,
    SparklinePanel,
    StatusDot,
)

log = structlog.get_logger("tui.link")


class LinkScreen(Screen):
    """WFB-ng video link status with gauges, sparklines, and stats."""

    DEFAULT_CSS = """
    LinkScreen {
        layout: vertical;
    }
    #link-top {
        height: 10;
    }
    #link-top-left {
        width: 30;
    }
    #link-top-right {
        width: 1fr;
    }
    #link-middle {
        height: 12;
    }
    #link-mid-left {
        width: 1fr;
    }
    #link-mid-right {
        width: 1fr;
    }
    #link-bottom-row {
        height: 1fr;
    }
    .link-col {
        width: 1fr;
    }
    """

    def compose(self) -> ComposeResult:
        # Top: connection info + RSSI sparkline
        with Horizontal(id="link-top"):
            with Vertical(id="link-top-left"):
                with InfoPanel("CONNECTION"):
                    yield StatusDot("Link", "unknown", id="link-state-dot")
                    yield Static("", id="link-iface-info")

            with Vertical(id="link-top-right"):
                yield SparklinePanel(
                    "RSSI HISTORY", maxlen=60, unit="dBm", id="spark-rssi"
                )

        # Middle: signal panel + throughput sparkline
        with Horizontal(id="link-middle"):
            with Vertical(id="link-mid-left"):
                with InfoPanel("SIGNAL"):
                    yield GaugeBar("RSSI", id="link-rssi-gauge", thresholds=(40.0, 70.0))
                    yield Static("", id="link-noise-info")
                    yield GaugeBar(
                        "SNR", id="link-snr-gauge",
                        thresholds=(999.0, 999.0), suffix="dB",
                    )

            with Vertical(id="link-mid-right"):
                yield SparklinePanel(
                    "THROUGHPUT", maxlen=60, unit="kbps", id="spark-bitrate"
                )

        # Bottom: 3 columns (packets, FEC, channel)
        with Horizontal(id="link-bottom-row"):
            with InfoPanel("PACKETS", classes="link-col"):
                yield Static("", id="link-pkt-info")

            with InfoPanel("FEC", classes="link-col"):
                yield Static("", id="link-fec-info")

            with InfoPanel("CHANNEL", classes="link-col"):
                yield Static("", id="link-chan-info")

    def on_mount(self) -> None:
        self.set_interval(1.0, self._refresh)

    async def _refresh(self) -> None:
        fetcher = self.app.fetcher  # type: ignore[attr-defined]
        data = await fetcher.get_wfb()

        if data is None:
            try:
                self.query_one("#link-state-dot", StatusDot).set_state("disconnected")
            except Exception:
                pass
            return

        state = data.get("state", "disabled")
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
        freq = data.get("frequency_mhz", 0)
        interface = data.get("interface", "N/A")
        restarts = data.get("restart_count", 0)

        # Push to ring buffers
        fetcher.push_sample("rssi", float(rssi))
        fetcher.push_sample("bitrate", float(bitrate))

        # -- Connection --
        try:
            self.query_one("#link-state-dot", StatusDot).set_state(state)
        except Exception:
            pass

        iface_text = (
            f"Interface: [#fafafa]{interface}[/#fafafa]\n"
            f"Channel:   [#3a82ff]{channel}[/#3a82ff]\n"
            f"Bitrate:   [#dff140]{bitrate} kbps[/#dff140]"
        )
        try:
            self.query_one("#link-iface-info", Static).update(iface_text)
        except Exception:
            pass

        # -- RSSI sparkline --
        try:
            spark = self.query_one("#spark-rssi", SparklinePanel)
            # Sparkline needs positive values. Shift RSSI: -100 -> 0, -30 -> 70
            shifted = max(0, rssi + 100)
            spark.push(shifted)
        except Exception:
            pass

        # -- Signal gauges --
        # RSSI gauge: map -100..-30 to 0..100
        rssi_pct = max(0, min(100, (rssi + 100) / 70 * 100))
        try:
            self.query_one("#link-rssi-gauge", GaugeBar).update_value(rssi_pct)
        except Exception:
            pass

        noise_text = (
            f"RSSI:  [#fafafa]{rssi:.1f} dBm[/#fafafa]\n"
            f"Noise: [#666666]{noise:.1f} dBm[/#666666]"
        )
        try:
            self.query_one("#link-noise-info", Static).update(noise_text)
        except Exception:
            pass

        # SNR gauge: map 0-40 to 0-100
        snr_pct = max(0, min(100, snr / 40 * 100))
        try:
            self.query_one("#link-snr-gauge", GaugeBar).update_value(snr_pct)
        except Exception:
            pass

        # -- Throughput sparkline --
        try:
            self.query_one("#spark-bitrate", SparklinePanel).push(float(bitrate))
        except Exception:
            pass

        # -- Packets --
        loss_color = "#22c55e" if loss < 1.0 else "#f59e0b" if loss < 5.0 else "#ef4444"
        pkt_text = (
            f"Received: [#fafafa]{pkts_rx:>10,}[/#fafafa]\n"
            f"Lost:     [#ef4444]{pkts_lost:>10,}[/#ef4444]\n"
            f"Loss:     [{loss_color}]{loss:>6.2f}%[/{loss_color}]"
        )
        try:
            self.query_one("#link-pkt-info", Static).update(pkt_text)
        except Exception:
            pass

        # -- FEC --
        fec_total = fec_rec + fec_fail
        fec_rate = (fec_rec / fec_total * 100) if fec_total > 0 else 0
        fec_color = "#22c55e" if fec_rate > 95 else "#f59e0b" if fec_rate > 80 else "#ef4444"

        fec_text = (
            f"Recovered: [#22c55e]{fec_rec:>8,}[/#22c55e]\n"
            f"Failed:    [#ef4444]{fec_fail:>8,}[/#ef4444]\n"
            f"Rate:      [{fec_color}]{fec_rate:>6.1f}%[/{fec_color}]"
        )
        try:
            self.query_one("#link-fec-info", Static).update(fec_text)
        except Exception:
            pass

        # -- Channel --
        freq_str = f"{freq} MHz" if freq else "N/A"
        chan_text = (
            f"Channel:  [#3a82ff]{channel}[/#3a82ff]\n"
            f"Freq:     [#fafafa]{freq_str}[/#fafafa]\n"
            f"Restarts: [#666666]{restarts}[/#666666]"
        )
        try:
            self.query_one("#link-chan-info", Static).update(chan_text)
        except Exception:
            pass
