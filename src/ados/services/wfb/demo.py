"""Demo WFB-ng manager — simulated link quality for testing without hardware."""

from __future__ import annotations

import asyncio
import math
import random
import time
from datetime import datetime, timezone

from ados.core.logging import get_logger
from ados.services.wfb.link_quality import LinkQualityMonitor, LinkStats
from ados.services.wfb.manager import LinkState

log = get_logger("wfb.demo")

# Simulation parameters
_BASE_RSSI = -55.0
_RSSI_AMPLITUDE = 15.0
_RSSI_PERIOD = 30.0  # seconds for one oscillation cycle
_BASE_NOISE = -95.0
_BASE_BITRATE = 8000  # kbps
_LOSS_RANGE = (0.1, 2.0)
_FEC_RECOVERY_RATE = 0.8
_PACKETS_PER_SECOND = 1200


class DemoWfbManager:
    """Simulated WFB-ng link for demo mode and testing.

    Generates realistic-looking link statistics with oscillating RSSI
    to simulate movement. Transitions through DISCONNECTED -> CONNECTING -> CONNECTED
    on startup, then generates stats at ~1 Hz.
    """

    def __init__(self) -> None:
        self._state = LinkState.DISCONNECTED
        self._monitor = LinkQualityMonitor()
        self._interface = "wlan_demo"
        self._channel = 149
        self._running = False
        self._start_time = 0.0

    @property
    def state(self) -> LinkState:
        """Current link state."""
        return self._state

    @property
    def interface(self) -> str:
        """Simulated interface name."""
        return self._interface

    @property
    def channel(self) -> int:
        """Simulated channel number."""
        return self._channel

    @property
    def monitor(self) -> LinkQualityMonitor:
        """Link quality monitor with stats history."""
        return self._monitor

    def get_status(self) -> dict:
        """Get current link status as a dictionary."""
        stats = self._monitor.get_current()
        return {
            "state": self._state.value,
            "interface": self._interface,
            "channel": self._channel,
            "rssi_dbm": stats.rssi_dbm,
            "noise_dbm": stats.noise_dbm,
            "snr_db": stats.snr_db,
            "packets_received": stats.packets_received,
            "packets_lost": stats.packets_lost,
            "loss_percent": stats.loss_percent,
            "fec_recovered": stats.fec_recovered,
            "fec_failed": stats.fec_failed,
            "bitrate_kbps": stats.bitrate_kbps,
            "restart_count": 0,
            "samples": self._monitor.sample_count,
        }

    def _generate_stats(self, elapsed: float) -> LinkStats:
        """Generate a realistic-looking LinkStats sample."""
        # RSSI oscillates to simulate drone moving closer/farther
        rssi_avg = _BASE_RSSI + _RSSI_AMPLITUDE * math.sin(
            2.0 * math.pi * elapsed / _RSSI_PERIOD
        )
        rssi_jitter = random.uniform(-2.0, 2.0)
        rssi_avg += rssi_jitter
        rssi_min = rssi_avg - random.uniform(2.0, 6.0)
        rssi_max = rssi_avg + random.uniform(2.0, 6.0)

        noise = _BASE_NOISE + random.uniform(-1.0, 1.0)
        snr = rssi_avg - noise

        # Packet stats
        packets = _PACKETS_PER_SECOND + random.randint(-50, 50)
        loss_pct = random.uniform(*_LOSS_RANGE)
        # Worse signal = more loss
        if rssi_avg < -70:
            loss_pct += (abs(rssi_avg) - 70) * 0.1
        lost = max(0, int(packets * loss_pct / 100.0))

        fec_total = lost + random.randint(0, 5)
        fec_recovered = int(fec_total * _FEC_RECOVERY_RATE)
        fec_failed = fec_total - fec_recovered

        # Bitrate varies slightly
        bitrate = _BASE_BITRATE + random.randint(-500, 500)

        total = packets + lost
        actual_loss = (lost / total * 100.0) if total > 0 else 0.0

        now = datetime.now(timezone.utc).isoformat()

        return LinkStats(
            rssi_dbm=round(rssi_avg, 1),
            rssi_min=round(rssi_min, 1),
            rssi_max=round(rssi_max, 1),
            noise_dbm=round(noise, 1),
            snr_db=round(snr, 1),
            packets_received=packets,
            packets_lost=lost,
            fec_recovered=fec_recovered,
            fec_failed=fec_failed,
            bitrate_kbps=bitrate,
            loss_percent=round(actual_loss, 2),
            timestamp=now,
        )

    async def stop(self) -> None:
        """Stop the demo manager."""
        self._running = False
        self._state = LinkState.DISCONNECTED
        log.info("demo_wfb_stopped")

    async def run(self) -> None:
        """Main demo loop — simulate WFB-ng link startup and operation."""
        self._running = True
        self._start_time = time.monotonic()
        log.info("demo_wfb_start", msg="Demo WFB-ng running on channel 149")

        # Simulate startup sequence
        self._state = LinkState.DISCONNECTED
        await asyncio.sleep(0.5)

        self._state = LinkState.CONNECTING
        await asyncio.sleep(1.0)

        self._state = LinkState.CONNECTED

        # Generate stats at ~1 Hz
        while self._running:
            elapsed = time.monotonic() - self._start_time
            stats = self._generate_stats(elapsed)
            self._monitor._latest = stats
            self._monitor._history.append(stats)
            self._monitor._timestamps.append(time.monotonic())

            # Occasional degraded state to make it realistic
            if stats.rssi_dbm < -68.0 or stats.loss_percent > 5.0:
                self._state = LinkState.DEGRADED
            else:
                self._state = LinkState.CONNECTED

            await asyncio.sleep(1.0)
