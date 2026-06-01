"""WFB-ng link quality monitoring and statistics parsing.

Parses the stdout stream that ``wfb_rx -l 1000`` emits once per second.
Upstream wfb-ng v26.4 prints a TAB-separated, colon-delimited format
(see ``referenceCode/fpv-video-link/wfb-ng/src/rx.cpp:495-508``):

    <ts_ms>\\tRX_ANT\\t<freq>:<mcs>:<bw>\\t<ant_id_hex>\\t<count>:<rssi_min>:<rssi_avg>:<rssi_max>:<snr_min>:<snr_avg>:<snr_max>
    <ts_ms>\\tPKT\\t<all_pkts>:<all_bytes>:<dec_err>:<session>:<data>:<uniq>:<fec_recovered>:<lost>:<bad>:<outgoing>:<bytes_outgoing>

One or more ``RX_ANT`` lines (one per antenna in use) precede a single
``PKT`` line per stats interval. We accumulate the most recent
``RX_ANT`` payload as we see it, and emit a unified ``LinkStats``
snapshot on each ``PKT`` arrival. This is the format wfb-ng has used
since v25; the previous regex-based parser (looking for
``rssi_min=N rssi_avg=N`` key=value pairs) is from a much older
release and silently never matched anything in production.
"""

from __future__ import annotations

import json
import os
import re
import tempfile
import time
from collections import deque
from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import StrEnum
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("wfb.link_quality")


class LinkState(StrEnum):
    """WFB-ng link connection state."""

    DISCONNECTED = "disconnected"
    UNPAIRED = "unpaired"
    AUTO_PAIRING = "auto_pairing"
    BINDING = "binding"
    CONNECTING = "connecting"
    CONNECTED = "connected"
    DEGRADED = "degraded"

# `\d+\tRX_ANT\t<freq>:<mcs>:<bw>\t<ant_id_hex>\t<count>:<rmin>:<ravg>:<rmax>:<smin>:<savg>:<smax>`
#
# Same trailing-tolerance rationale as the PKT pattern below: pin only
# the leading fields we read and ignore any extra trailing counters a
# newer receiver build appends, so a format bump never silently zeroes
# the RSSI/SNR display.
_RX_ANT_RE = re.compile(
    r"^\d+\tRX_ANT\t"
    r"(?P<freq>\d+):(?P<mcs>\d+):(?P<bw>\d+)\t"
    r"(?P<ant_id>[0-9a-fA-F]+)\t"
    r"(?P<count>\d+):"
    r"(?P<rssi_min>-?\d+):(?P<rssi_avg>-?\d+):(?P<rssi_max>-?\d+):"
    r"(?P<snr_min>-?\d+):(?P<snr_avg>-?\d+):(?P<snr_max>-?\d+)(?::-?\d+)*$"
)

# `\d+\tPKT\t<p_all>:<b_all>:<dec_err>:<sess>:<data>:<uniq>:<fec_rec>:<lost>:<bad>:<out>:<b_out>`
#
# The trailing match is intentionally NOT $-anchored after b_outgoing:
# different builds of the receiver append extra counters to the PKT
# line (e.g. a bad_session or pkt_drop field) over time. Anchoring to
# exactly 11 fields means a build with a 12th field fails the whole
# match, the receiver emits no stats, and packets_received / bitrate
# silently read zero while video decodes fine. We pin only the leading
# fields we consume and tolerate (ignore) any extra trailing
# colon-separated counters.
_PKT_RE = re.compile(
    r"^\d+\tPKT\t"
    r"(?P<p_all>\d+):"
    r"(?P<b_all>\d+):"
    r"(?P<dec_err>\d+):"
    r"(?P<session>\d+):"
    r"(?P<data>\d+):"
    r"(?P<uniq>\d+):"
    r"(?P<fec_rec>\d+):"
    r"(?P<lost>\d+):"
    r"(?P<bad>\d+):"
    r"(?P<outgoing>\d+):"
    r"(?P<b_outgoing>\d+)(?::\d+)*$"
)

DEFAULT_HISTORY_SIZE = 300

# Default stats interval — must match the value passed to wfb_rx -l.
# The agent currently spawns wfb_rx with `-l 1000` on both profiles,
# so each PKT line summarises one second of receive activity. If the
# interval changes, override via LinkQualityMonitor(stats_interval_s=…).
_DEFAULT_STATS_INTERVAL_S = 1.0


@dataclass
class RxAntSnapshot:
    """Latest per-antenna sample from a single RX_ANT line."""

    freq_mhz: int = 0
    mcs_index: int = 0
    bandwidth_mhz: int = 0
    antenna_id: int = 0
    count: int = 0
    rssi_min: int = -100
    rssi_avg: int = -100
    rssi_max: int = -100
    snr_min: int = 0
    snr_avg: int = 0
    snr_max: int = 0


@dataclass
class PktSnapshot:
    """Aggregate per-interval counters from a single PKT line."""

    count_p_all: int = 0
    count_b_all: int = 0
    count_p_dec_err: int = 0
    count_p_session: int = 0
    count_p_data: int = 0
    count_p_uniq: int = 0
    count_p_fec_recovered: int = 0
    count_p_lost: int = 0
    count_p_bad: int = 0
    count_p_outgoing: int = 0
    count_b_outgoing: int = 0


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


def parse_rx_ant_line(line: str) -> RxAntSnapshot | None:
    """Parse a single RX_ANT stats line. Returns None on no match."""
    match = _RX_ANT_RE.match(line.rstrip())
    if match is None:
        return None
    g = match.groupdict()
    try:
        return RxAntSnapshot(
            freq_mhz=int(g["freq"]),
            mcs_index=int(g["mcs"]),
            bandwidth_mhz=int(g["bw"]),
            antenna_id=int(g["ant_id"], 16),
            count=int(g["count"]),
            rssi_min=int(g["rssi_min"]),
            rssi_avg=int(g["rssi_avg"]),
            rssi_max=int(g["rssi_max"]),
            snr_min=int(g["snr_min"]),
            snr_avg=int(g["snr_avg"]),
            snr_max=int(g["snr_max"]),
        )
    except (ValueError, TypeError) as exc:
        log.debug("rx_ant_parse_failed", line=line[:120], error=str(exc))
        return None


def parse_pkt_line(line: str) -> PktSnapshot | None:
    """Parse a single PKT stats line. Returns None on no match."""
    match = _PKT_RE.match(line.rstrip())
    if match is None:
        return None
    g = match.groupdict()
    try:
        return PktSnapshot(
            count_p_all=int(g["p_all"]),
            count_b_all=int(g["b_all"]),
            count_p_dec_err=int(g["dec_err"]),
            count_p_session=int(g["session"]),
            count_p_data=int(g["data"]),
            count_p_uniq=int(g["uniq"]),
            count_p_fec_recovered=int(g["fec_rec"]),
            count_p_lost=int(g["lost"]),
            count_p_bad=int(g["bad"]),
            count_p_outgoing=int(g["outgoing"]),
            count_b_outgoing=int(g["b_outgoing"]),
        )
    except (ValueError, TypeError) as exc:
        log.debug("pkt_parse_failed", line=line[:120], error=str(exc))
        return None


def parse_wfb_rx_line(line: str) -> LinkStats | None:
    """Backwards-compatible single-line parser (legacy callers).

    Modern callers should use ``LinkQualityMonitor.feed_line()`` which
    properly aggregates an RX_ANT line with the following PKT line.
    This helper handles the case where a caller passes a self-contained
    line and expects an immediate result; it returns a partially-filled
    LinkStats from whichever line type matches, or None if neither.
    """
    rx = parse_rx_ant_line(line)
    if rx is not None:
        snr = float(rx.snr_avg)
        # Approximate noise floor from rssi - snr (radiotap doesn't
        # publish noise on most RTL adapters; this is the standard
        # reconstruction used by wfb-cli's display layer).
        noise = float(rx.rssi_avg) - snr
        return LinkStats(
            rssi_dbm=float(rx.rssi_avg),
            rssi_min=float(rx.rssi_min),
            rssi_max=float(rx.rssi_max),
            noise_dbm=noise,
            snr_db=snr,
            timestamp=datetime.now(timezone.utc).isoformat(),
        )
    pkt = parse_pkt_line(line)
    if pkt is not None:
        return LinkStats(
            packets_received=pkt.count_p_data,
            packets_lost=pkt.count_p_lost,
            fec_recovered=pkt.count_p_fec_recovered,
            fec_failed=pkt.count_p_lost,  # in upstream "lost" = beyond FEC
            timestamp=datetime.now(timezone.utc).isoformat(),
        )
    return None


@dataclass
class LinkQualityMonitor:
    """Rolling buffer of link quality samples from wfb_rx.

    Stateful aggregator: tracks the latest RX_ANT payload and emits a
    fully populated LinkStats only when the corresponding PKT line
    arrives (one per stats interval). This matches the upstream
    wfb-ng v26.4 stdout shape where one or more RX_ANT lines precede
    the PKT line for the same interval.

    Maintains a ring buffer of the last N samples (default 300) for
    graphing in setup surfaces and the API history endpoint.
    """

    max_samples: int = DEFAULT_HISTORY_SIZE
    stats_interval_s: float = _DEFAULT_STATS_INTERVAL_S
    _history: deque[LinkStats] = field(default_factory=deque)
    _latest: LinkStats = field(default_factory=LinkStats)
    _timestamps: deque[float] = field(default_factory=deque)
    # Aggregator state — most recent RX_ANT seen this interval, and
    # the previous PKT line's outgoing-byte counter so we can derive
    # bitrate from the delta. wfb_rx's PKT counters are per-interval
    # (cleared on each emission, see rx.cpp:521 ``clear_stats()``), so
    # bitrate = count_b_outgoing / interval directly without delta math.
    _last_rx_ant: RxAntSnapshot | None = field(default=None)
    _last_pkt: PktSnapshot | None = field(default=None)
    # Last time we logged an unmatched line. Rate-limits the noisy debug
    # log so a wfb-ng format drift surfaces in journalctl without spam.
    _last_unmatched_log: float = 0.0

    def __post_init__(self) -> None:
        self._history = deque(maxlen=self.max_samples)
        self._timestamps = deque(maxlen=self.max_samples)

    def feed_line(self, line: str) -> LinkStats | None:
        """Parse a wfb_rx output line and emit a LinkStats snapshot.

        Both line types now emit a snapshot so a consumer (the stats-file
        writer, REST, the LCD) always gets a fresh, fully-populated view
        on every stats line:

        * RX_ANT line: updates RSSI/SNR and emits a snapshot that
          carries FORWARD the most recent PKT counters (packets, bitrate,
          loss). Previously RX_ANT returned None, so on an interval whose
          PKT line failed to parse (receiver-build field drift) the whole
          snapshot was dropped — RSSI/SNR went stale and the stats file
          was never refreshed even though decodes were flowing.
        * PKT line: updates the packet/bitrate/loss counters and emits a
          snapshot combining them with the latest RX_ANT data.

        Lines that match neither shape return None silently.
        """
        rx = parse_rx_ant_line(line)
        if rx is not None:
            self._last_rx_ant = rx
            # Emit a snapshot now, carrying forward the last PKT counters
            # so RSSI/SNR freshness (and the stats-file write that rides
            # on a non-None return) never depends on the matching PKT
            # line for this interval also parsing.
            return self._emit(self._last_pkt, rx)

        pkt = parse_pkt_line(line)
        if pkt is None:
            # Surface format drift in journalctl. Without this, a
            # receiver release that reshapes the stdout stats lines
            # produces zero stats with no log signal — consumers
            # downstream (LCD, REST, heartbeat) all silently render
            # blank values. Rate-limit to one log every 5 s so a
            # genuine spew doesn't drown the journal.
            stripped = line.strip()
            if stripped:
                now = time.monotonic()
                if now - self._last_unmatched_log > 5.0:
                    log.debug("wfb_rx_no_match", line=stripped[:120])
                    self._last_unmatched_log = now
            return None

        self._last_pkt = pkt
        return self._emit(pkt, self._last_rx_ant)

    def _emit(
        self, pkt: PktSnapshot | None, rx: RxAntSnapshot | None
    ) -> LinkStats:
        """Build, store, and return a snapshot from the latest counters."""
        stats = self._build_stats(pkt, rx)
        self._latest = stats
        self._history.append(stats)
        self._timestamps.append(time.monotonic())
        log.debug(
            "link_stats_updated",
            rssi=stats.rssi_dbm,
            loss=stats.loss_percent,
            packets=stats.packets_received,
            bitrate=stats.bitrate_kbps,
        )
        return stats

    def _build_stats(
        self, pkt: PktSnapshot | None, rx: RxAntSnapshot | None
    ) -> LinkStats:
        """Combine the latest PKT + RX_ANT counters into LinkStats.

        ``pkt`` may be None when an RX_ANT line arrives before the first
        PKT line of the session has parsed; in that case the packet /
        bitrate / loss fields read as zero while RSSI/SNR still reflect
        the live RX_ANT. Once a PKT line has been seen its counters are
        carried forward, so the packet/bitrate fields stay populated on
        an RX_ANT-only emit.
        """
        if rx is not None:
            rssi_avg = float(rx.rssi_avg)
            rssi_min = float(rx.rssi_min)
            rssi_max = float(rx.rssi_max)
            snr = float(rx.snr_avg)
            # RTL adapters don't publish noise; reconstruct as rssi - snr.
            noise = rssi_avg - snr
        else:
            rssi_avg = -100.0
            rssi_min = -100.0
            rssi_max = -100.0
            snr = 0.0
            noise = -95.0

        if pkt is None:
            pkt = PktSnapshot()

        # bitrate = bytes-forwarded-this-interval * 8 / interval. Each
        # PKT line covers one interval (default 1 s via `-l 1000`).
        bitrate_kbps = int(
            pkt.count_b_outgoing * 8.0 / self.stats_interval_s / 1000.0
        )

        # loss% over the data + lost denominator (so dec_err and bad
        # don't dominate when keys are mismatched and every packet
        # fails AEAD; the "lost" counter alone is the operational
        # signal we care about).
        denominator = pkt.count_p_data + pkt.count_p_lost
        loss_pct = (
            (pkt.count_p_lost / denominator * 100.0)
            if denominator > 0
            else 0.0
        )

        now_iso = datetime.now(timezone.utc).isoformat()
        return LinkStats(
            rssi_dbm=rssi_avg,
            rssi_min=rssi_min,
            rssi_max=rssi_max,
            noise_dbm=noise,
            snr_db=snr,
            packets_received=pkt.count_p_data,
            packets_lost=pkt.count_p_lost,
            fec_recovered=pkt.count_p_fec_recovered,
            fec_failed=pkt.count_p_lost,
            bitrate_kbps=bitrate_kbps,
            loss_percent=round(loss_pct, 2),
            timestamp=now_iso,
        )

    def get_current(self) -> LinkStats:
        """Return the most recent link stats sample."""
        return self._latest

    def get_history(self, seconds: int = 60) -> list[LinkStats]:
        """Return link stats from the last N seconds, oldest-first."""
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
        return len(self._history)

    def clear(self) -> None:
        """Reset all stored samples and aggregator state."""
        self._history.clear()
        self._timestamps.clear()
        self._latest = LinkStats()
        self._last_rx_ant = None
        self._last_pkt = None

    def persist_to_file(
        self,
        path: Path,
        *,
        extra: dict | None = None,
    ) -> None:
        """Atomically write the current snapshot to ``path`` as JSON.

        Used by both WfbManager (drone) and WfbRxManager (GS) to expose
        live radio stats to other agent processes (the API server, the
        OLED service) that don't share memory with the wfb subprocess.
        ``extra`` lets the caller mix in profile-specific fields like
        the configured channel or topology so consumers get a single
        unified view.

        Atomic via tmpfile + rename so a partial read is never possible.
        """
        payload = self._latest.to_dict()
        payload["state"] = (
            "connected" if self._latest.packets_received > 0 else "connecting"
        )
        payload["samples"] = self.sample_count
        if extra:
            payload.update(extra)
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            with tempfile.NamedTemporaryFile(
                mode="w",
                dir=str(path.parent),
                delete=False,
                prefix=f".{path.name}.",
                suffix=".tmp",
            ) as tmp:
                json.dump(payload, tmp)
                tmp_path = Path(tmp.name)
            os.replace(str(tmp_path), str(path))
        except OSError as exc:
            log.debug(
                "wfb_stats_persist_failed",
                path=str(path),
                error=str(exc),
            )
