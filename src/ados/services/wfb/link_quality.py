"""WFB-ng link quality monitoring and statistics parsing."""

from __future__ import annotations

import re
import time
from collections import deque
from dataclasses import dataclass, field
from datetime import datetime, timezone

from ados.core.logging import get_logger

log = get_logger("wfb.link_quality")

# WFB-ng rx stats line pattern:
# RX ANT 0: [addr] rssi_min=-52 rssi_avg=-48 rssi_max=-44 packets=1234 lost=2 fec_rec=5 fec_fail=0
_RX_STATS_RE = re.compile(
    r"rssi_min=(-?\d+)\s+"
    r"rssi_avg=(-?\d+)\s+"
    r"rssi_max=(-?\d+)\s+"
    r"packets=(\d+)\s+"
    r"lost=(\d+)\s+"
    r"fec_rec=(\d+)\s+"
    r"fec_fail=(\d+)"
)

# Bitrate line: "bitrate: 1234 kbit/s" or "RX: NNN bytes NNN packets NNN kbit/s"
_BITRATE_RE = re.compile(r"(\d+)\s*kbit/s")

# Noise floor line (some WFB-ng versions): "noise=-95"
_NOISE_RE = re.compile(r"noise=(-?\d+)")

DEFAULT_HISTORY_SIZE = 300


@dataclass
class LinkStats:
    """Snapshot of WFB-ng link quality at a point in time."""

    rssi_dbm: float = -100.0
    rssi_min: float = -100.0
    rssi_max: float = -100.0
    noise_dbm: float = -95.0
    snr_db: float = 0.0
    packets_received: int = 0
    packets_lost: int = 0
    fec_recovered: int = 0
    fec_failed: int = 0
    bitrate_kbps: int = 0
    loss_percent: float = 0.0
    timestamp: str = ""

    def to_dict(self) -> dict:
        """Serialize to dictionary for API responses."""
        return {
            "rssi_dbm": self.rssi_dbm,
            "rssi_min": self.rssi_min,
            "rssi_max": self.rssi_max,
            "noise_dbm": self.noise_dbm,
            "snr_db": self.snr_db,
            "packets_received": self.packets_received,
            "packets_lost": self.packets_lost,
            "fec_recovered": self.fec_recovered,
            "fec_failed": self.fec_failed,
            "bitrate_kbps": self.bitrate_kbps,
            "loss_percent": self.loss_percent,
            "timestamp": self.timestamp,
        }


def parse_wfb_rx_line(line: str) -> LinkStats | None:
    """Parse a single line of wfb_rx stdout output into LinkStats.

    Returns None if the line does not contain stats information.
    """
    match = _RX_STATS_RE.search(line)
    if not match:
        return None

    rssi_min = float(match.group(1))
    rssi_avg = float(match.group(2))
    rssi_max = float(match.group(3))
    packets = int(match.group(4))
    lost = int(match.group(5))
    fec_rec = int(match.group(6))
    fec_fail = int(match.group(7))

    # Extract noise if present
    noise_match = _NOISE_RE.search(line)
    noise = float(noise_match.group(1)) if noise_match else -95.0

    snr = rssi_avg - noise

    # Extract bitrate if present
    bitrate_match = _BITRATE_RE.search(line)
    bitrate = int(bitrate_match.group(1)) if bitrate_match else 0

    total = packets + lost
    loss_pct = (lost / total * 100.0) if total > 0 else 0.0

    now = datetime.now(timezone.utc).isoformat()

    return LinkStats(
        rssi_dbm=rssi_avg,
        rssi_min=rssi_min,
        rssi_max=rssi_max,
        noise_dbm=noise,
        snr_db=snr,
        packets_received=packets,
        packets_lost=lost,
        fec_recovered=fec_rec,
        fec_failed=fec_fail,
        bitrate_kbps=bitrate,
        loss_percent=round(loss_pct, 2),
        timestamp=now,
    )


@dataclass
class LinkQualityMonitor:
    """Rolling buffer of link quality samples from wfb_rx.

    Maintains a ring buffer of the last N samples (default 300) for
    graphing in the TUI and API history endpoint.
    """

    max_samples: int = DEFAULT_HISTORY_SIZE
    _history: deque[LinkStats] = field(default_factory=deque)
    _latest: LinkStats = field(default_factory=LinkStats)
    _timestamps: deque[float] = field(default_factory=deque)

    def __post_init__(self) -> None:
        self._history = deque(maxlen=self.max_samples)
        self._timestamps = deque(maxlen=self.max_samples)

    def feed_line(self, line: str) -> LinkStats | None:
        """Parse a wfb_rx output line and store if valid.

        Returns the parsed LinkStats if the line contained stats, None otherwise.
        """
        stats = parse_wfb_rx_line(line)
        if stats is not None:
            self._latest = stats
            self._history.append(stats)
            self._timestamps.append(time.monotonic())
            log.debug(
                "link_stats_updated",
                rssi=stats.rssi_dbm,
                loss=stats.loss_percent,
                packets=stats.packets_received,
            )
        return stats

    def get_current(self) -> LinkStats:
        """Return the most recent link stats sample."""
        return self._latest

    def get_history(self, seconds: int = 60) -> list[LinkStats]:
        """Return link stats from the last N seconds.

        Args:
            seconds: How many seconds of history to return.

        Returns:
            List of LinkStats ordered oldest-first.
        """
        if not self._history:
            return []

        cutoff = time.monotonic() - seconds
        result: list[LinkStats] = []
        for i, ts in enumerate(self._timestamps):
            if ts >= cutoff:
                result.append(self._history[i])
        return result

    @property
    def sample_count(self) -> int:
        """Number of samples in the buffer."""
        return len(self._history)

    def clear(self) -> None:
        """Clear all stored samples."""
        self._history.clear()
        self._timestamps.clear()
        self._latest = LinkStats()
