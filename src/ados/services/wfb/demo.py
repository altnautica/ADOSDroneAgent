"""Demo WFB-ng manager — simulated link quality for testing without hardware."""

from __future__ import annotations

import asyncio
import math
import random
import time
from datetime import datetime, timezone

from ados.core.logging import get_logger
from ados.services.wfb.link_quality import LinkQualityMonitor, LinkState, LinkStats

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

# A measured link is called bad only on a real reading, so these gate the
# DEGRADED verdict on the generated stats rather than on the proof below.
_DEGRADED_RSSI_DBM = -68.0
_DEGRADED_LOSS_PERCENT = 5.0

# Simulated received-side proof cadence.
#
# The drone is the video source: once the link is up it is always injecting,
# so the verdict turns entirely on whether a return signal was heard recently.
# That is the real trigger, so the simulation models the same observable — the
# age of the last return signal — instead of flipping a flag. The simulated
# peer answers for the first stretch of each cycle and then goes quiet; the
# proof ages past the grace window part-way through the silence, and the link
# reports rf_unverified until the peer is heard again.
#
# The grace window is compressed from the radio's 30 s so an episode is
# reachable inside a short demo session; the shape of the derivation is
# unchanged.
_RX_PROOF_CYCLE = 40.0  # seconds for one heard -> silent -> heard cycle
_RX_PROOF_HEARD_WINDOW = 22.0  # the peer answers this far into each cycle
_RX_PROOF_GRACE = 6.0  # proof older than this is no longer proof


def _seconds_since_return_signal(elapsed: float) -> float:
    """Age of the simulated return signal at ``elapsed`` seconds.

    Zero while the simulated peer is answering (a beacon just landed), then
    climbing once it goes quiet, so callers read the same observable the radio
    tracks: how long since a verified return signal was last heard.
    """
    phase = elapsed % _RX_PROOF_CYCLE
    if phase < _RX_PROOF_HEARD_WINDOW:
        return 0.0
    return phase - _RX_PROOF_HEARD_WINDOW


def _is_rf_unverified(tx_live: bool, rx_proven: bool) -> bool:
    """Transmitting with no confirmed reception — the radio's own verdict.

    Not unverified when the transmit counter is flat (that is the idle case)
    or when a return signal is fresh (the link is proven).
    """
    return tx_live and not rx_proven


class DemoWfbManager:
    """Simulated WFB-ng link for demo mode and testing.

    Generates realistic-looking link statistics with oscillating RSSI
    to simulate movement. Transitions through DISCONNECTED -> CONNECTING -> CONNECTED
    on startup, then generates stats at ~1 Hz.

    The data is simulated, and says so: the interface reports ``wlan_demo``
    (no such device exists) and nothing here touches the wfb-stats sidecar the
    real radio writes, so a demo reading can never be mistaken for a rig's.

    The received-side proof is simulated alongside the stats so the
    transmitting-with-no-confirmed-reception path is exercisable without
    hardware. ``channel_locked`` and ``rf_unverified`` are two views of the one
    simulated proof, as they are on the real radio, so a status body can never
    report an unverified link as locked.
    """

    def __init__(self) -> None:
        self._state = LinkState.DISCONNECTED
        self._monitor = LinkQualityMonitor()
        self._interface = "wlan_demo"
        self._channel = 149
        self._running = False
        self._start_time = 0.0
        # Received-side proof, simulated. `_tx_live` mirrors an advancing
        # transmit counter (false until the link is up, and again once
        # stopped); `_rx_proven` mirrors a return signal heard inside the
        # grace window. Both feed the one verdict below.
        self._tx_live = False
        self._rx_proven = False
        # Surface the same TX power knobs as the real manager so the
        # routes layer can introspect them uniformly during tests.
        self._tx_power_dbm: int = 5
        self._tx_power_max_dbm: int = 15
        self._mcs_index: int = 1
        self._topology: str = "host_vbus"

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

    @property
    def rf_unverified(self) -> bool:
        """Transmitting with no confirmed reception, from the simulated proof."""
        return _is_rf_unverified(self._tx_live, self._rx_proven)

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
            "tx_power_dbm": self._tx_power_dbm,
            "tx_power_max_dbm": self._tx_power_max_dbm,
            "mcs_index": self._mcs_index,
            "topology": self._topology,
            # The two halves of the received-side proof, both derived from the
            # one simulated `_rx_proven` so they agree with each other and with
            # the state string: locked once a return signal was heard,
            # rf_unverified while the transmit counter advances and none has
            # been.
            "channel_locked": self._rx_proven,
            "rf_unverified": self.rf_unverified,
        }

    @property
    def effective_tx_power_dbm(self) -> int | None:
        """Last accepted TX power in dBm — demo always reports the stored value."""
        return self._tx_power_dbm

    def apply_tx_power(self, dbm: int) -> int | None:
        """Pretend to apply a TX power. Mirrors the real manager's clamp."""
        clamped = max(1, min(int(dbm), self._tx_power_max_dbm))
        self._tx_power_dbm = clamped
        return clamped

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

    def _derive_state(self, stats: LinkStats, rx_proven: bool) -> LinkState:
        """Rank the simulated link the way the radio ranks the real one.

        A genuinely bad MEASURED link outranks an unproven one, so DEGRADED is
        decided first and an unverified transmit path is never masked over a
        real reading. Below it, injecting with no confirmed reception is
        RF_UNVERIFIED rather than CONNECTED: an advancing transmit counter only
        proves frames were accepted, never that the energy reached a receiver.
        """
        if (
            stats.rssi_dbm < _DEGRADED_RSSI_DBM
            or stats.loss_percent > _DEGRADED_LOSS_PERCENT
        ):
            return LinkState.DEGRADED
        if _is_rf_unverified(self._tx_live, rx_proven):
            return LinkState.RF_UNVERIFIED
        return LinkState.CONNECTED

    def _tick(self, elapsed: float) -> LinkStats:
        """Advance the simulation one sample and settle the derived state.

        Split out of the run loop so the whole hear -> lose -> hear cycle can
        be driven from a test clock instead of a wall-clock sleep.
        """
        stats = self._generate_stats(elapsed)
        self._monitor._latest = stats
        self._monitor._history.append(stats)
        self._monitor._timestamps.append(time.monotonic())

        proof_age = _seconds_since_return_signal(elapsed)
        self._rx_proven = proof_age <= _RX_PROOF_GRACE
        was_unverified = self._state == LinkState.RF_UNVERIFIED
        self._state = self._derive_state(stats, self._rx_proven)

        is_unverified = self._state == LinkState.RF_UNVERIFIED
        if is_unverified != was_unverified:
            log.info(
                "demo_wfb_rf_unverified_entry" if is_unverified
                else "demo_wfb_rf_unverified_clear",
                proof_age_s=round(proof_age, 1),
            )
        return stats

    async def stop(self) -> None:
        """Stop the demo manager."""
        self._running = False
        self._state = LinkState.DISCONNECTED
        # Nothing is injecting once stopped, so the transmitting-with-no-
        # reception verdict clears with it rather than sticking at its last
        # value.
        self._tx_live = False
        self._rx_proven = False
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
        # The drone is the video source, so from here it is always injecting.
        self._tx_live = True

        # Generate stats at ~1 Hz
        while self._running:
            self._tick(time.monotonic() - self._start_time)
            await asyncio.sleep(1.0)
