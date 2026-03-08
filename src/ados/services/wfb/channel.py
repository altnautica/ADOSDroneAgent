"""WFB-ng channel management — scanning, selection, and switching."""

from __future__ import annotations

import platform
import re
import subprocess
from dataclasses import dataclass

from ados.core.logging import get_logger

log = get_logger("wfb.channel")


@dataclass
class WfbChannel:
    """A WiFi channel usable by WFB-ng."""

    frequency_mhz: int
    channel_number: int
    bandwidth_mhz: int = 20


# Standard 5 GHz channels commonly used with WFB-ng and RTL8812AU/BU.
# These channels are typically available in most regulatory domains.
STANDARD_CHANNELS: list[WfbChannel] = [
    WfbChannel(frequency_mhz=5180, channel_number=36),
    WfbChannel(frequency_mhz=5240, channel_number=48),
    WfbChannel(frequency_mhz=5745, channel_number=149),
    WfbChannel(frequency_mhz=5765, channel_number=153),
    WfbChannel(frequency_mhz=5785, channel_number=157),
    WfbChannel(frequency_mhz=5805, channel_number=161),
    WfbChannel(frequency_mhz=5825, channel_number=165),
]

# Quick lookup by channel number
_CHANNEL_MAP: dict[int, WfbChannel] = {ch.channel_number: ch for ch in STANDARD_CHANNELS}


def get_channel(channel_number: int) -> WfbChannel | None:
    """Look up a WfbChannel by channel number. Returns None if unknown."""
    return _CHANNEL_MAP.get(channel_number)


def _parse_scan_results(output: str) -> list[tuple[int, int]]:
    """Parse `iw scan` output to extract frequency and signal strength.

    Returns list of (frequency_mhz, signal_dbm) tuples for detected networks.
    """
    results: list[tuple[int, int]] = []
    current_freq = 0

    freq_re = re.compile(r"freq:\s*(\d+)")
    signal_re = re.compile(r"signal:\s*(-?\d+(?:\.\d+)?)\s*dBm")

    for line in output.splitlines():
        stripped = line.strip()
        freq_match = freq_re.match(stripped)
        if freq_match:
            current_freq = int(freq_match.group(1))
            continue

        signal_match = signal_re.match(stripped)
        if signal_match and current_freq > 0:
            signal = int(float(signal_match.group(1)))
            results.append((current_freq, signal))
            current_freq = 0

    return results


def scan_channels(interface: str) -> list[tuple[WfbChannel, int]]:
    """Scan for WiFi networks and measure interference on WFB-ng channels.

    Uses `iw <interface> scan` to detect nearby access points. For each
    standard WFB-ng channel, counts how many APs are operating there.
    Returns a list of (channel, network_count) tuples sorted by least congested.

    Must be run as root on Linux. Interface should be in managed mode for scanning.
    On non-Linux platforms, returns all channels with zero interference.
    """
    system = platform.system()
    if system != "Linux":
        log.warning("channel_scan_unsupported", platform=system)
        return [(ch, 0) for ch in STANDARD_CHANNELS]

    try:
        result = subprocess.run(
            ["iw", interface, "scan"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode != 0:
            log.warning(
                "channel_scan_failed",
                returncode=result.returncode,
                stderr=result.stderr.strip()[:200],
            )
            return [(ch, 0) for ch in STANDARD_CHANNELS]
    except FileNotFoundError:
        log.warning("iw_not_found")
        return [(ch, 0) for ch in STANDARD_CHANNELS]
    except subprocess.TimeoutExpired:
        log.warning("channel_scan_timeout")
        return [(ch, 0) for ch in STANDARD_CHANNELS]

    detected = _parse_scan_results(result.stdout)

    # Count APs per standard channel (within 20 MHz bandwidth window)
    channel_interference: dict[int, int] = {ch.channel_number: 0 for ch in STANDARD_CHANNELS}
    for freq, _signal in detected:
        for ch in STANDARD_CHANNELS:
            if abs(freq - ch.frequency_mhz) <= ch.bandwidth_mhz:
                channel_interference[ch.channel_number] += 1

    results = [
        (ch, channel_interference[ch.channel_number])
        for ch in STANDARD_CHANNELS
    ]
    results.sort(key=lambda x: x[1])

    log.info(
        "channel_scan_complete",
        interface=interface,
        networks_found=len(detected),
        least_congested=results[0][0].channel_number if results else 0,
    )
    return results


def select_best_channel(scan_results: list[tuple[WfbChannel, int]]) -> WfbChannel:
    """Pick the least congested channel from scan results.

    If scan_results is empty, defaults to channel 149 (5745 MHz),
    which is the most commonly used WFB-ng channel.
    """
    if not scan_results:
        default = _CHANNEL_MAP[149]
        log.info("channel_default_selected", channel=149)
        return default

    # Already sorted by interference count (ascending) from scan_channels
    best = scan_results[0][0]
    log.info(
        "channel_selected",
        channel=best.channel_number,
        frequency=best.frequency_mhz,
        interference=scan_results[0][1],
    )
    return best


def set_channel(interface: str, channel: int) -> bool:
    """Set the WiFi interface to a specific channel number.

    Uses `iw <interface> set channel <N>`. The interface must be in
    monitor mode before calling this.
    Returns True on success.
    """
    system = platform.system()
    if system != "Linux":
        log.warning("set_channel_unsupported", platform=system)
        return False

    if channel not in _CHANNEL_MAP:
        log.error("invalid_channel", channel=channel, valid=list(_CHANNEL_MAP.keys()))
        return False

    try:
        result = subprocess.run(
            ["iw", interface, "set", "channel", str(channel)],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode != 0:
            log.error(
                "set_channel_failed",
                channel=channel,
                stderr=result.stderr.strip(),
            )
            return False
    except FileNotFoundError:
        log.error("iw_not_found")
        return False
    except subprocess.TimeoutExpired:
        log.error("set_channel_timeout", channel=channel)
        return False

    log.info("channel_set", interface=interface, channel=channel)
    return True
