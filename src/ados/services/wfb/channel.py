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


# Standard 5 GHz channels commonly used with WFB-ng on the RTL8812 family.
# Two contiguous sub-bands per regulatory domain: U-NII-1 (5180-5240,
# channels 36/40/44/48) and U-NII-3 (5745-5825, channels 149-161 plus
# 165). U-NII-1 is almost always quieter than U-NII-3 indoors because
# consumer routers default to 149-161; the auto-channel selector
# defaults to the U-NII-1 band for that reason.
STANDARD_CHANNELS: list[WfbChannel] = [
    WfbChannel(frequency_mhz=5180, channel_number=36),
    WfbChannel(frequency_mhz=5200, channel_number=40),
    WfbChannel(frequency_mhz=5220, channel_number=44),
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


# Band whitelists for `select_quietest_channel`. The U-NII-1 sub-band
# is almost always quieter than U-NII-3 in a home/office because
# consumer routers default to 149-161. Operators can pick a band per
# regulatory regime; if the chosen band turns out to be empty after
# filtering, we fall back to ALL standard channels.
_BAND_CHANNELS: dict[str, tuple[int, ...]] = {
    "u-nii-1": (36, 40, 44, 48),
    "u-nii-3": (149, 153, 157, 161, 165),
    "all": tuple(ch.channel_number for ch in STANDARD_CHANNELS),
}


def select_quietest_channel(
    interface: str,
    band: str = "u-nii-1",
) -> WfbChannel:
    """Scan once and return the quietest channel inside ``band``.

    ``band`` is one of ``u-nii-1``, ``u-nii-3``, or ``all``. The match
    is case-insensitive and tolerates the dotted spelling our config
    accepts. If a band whitelist removes every channel from the scan
    results (e.g., the rig's regulatory domain forbids that sub-band),
    we fall back to all standard channels rather than refusing.

    The returned channel is suitable to pass to ``set_channel()`` /
    write into ``WfbConfig.channel`` for both rigs in a pair so they
    independently bring up wfb_tx / wfb_rx on a quiet frequency.

    The function blocks for up to 30 s while ``iw scan`` runs (see
    ``scan_channels``). It is intended to be called once per bind
    cycle, NOT on every link health tick.
    """
    band_key = band.replace("-", "").replace(".", "").lower()
    band_lookup = {
        "unii1": _BAND_CHANNELS["u-nii-1"],
        "unii3": _BAND_CHANNELS["u-nii-3"],
        "all": _BAND_CHANNELS["all"],
    }
    allowed = band_lookup.get(band_key, _BAND_CHANNELS["u-nii-1"])
    scan_all = scan_channels(interface)
    filtered = [
        (ch, cnt) for ch, cnt in scan_all if ch.channel_number in allowed
    ]
    # Re-sort after filtering so select_best_channel's "first wins" rule
    # picks the quietest channel within the band rather than the first
    # band-allowed channel that happened to appear in the unfiltered
    # scan order. scan_channels already sorts globally but the filter
    # may drop entries between two band-allowed ones.
    filtered.sort(key=lambda pair: pair[1])
    if not filtered:
        log.warning(
            "channel_band_filter_empty",
            interface=interface,
            band=band,
            note="band whitelist removed all channels; falling back to all",
        )
        filtered = sorted(scan_all, key=lambda pair: pair[1])
    best = select_best_channel(filtered)
    log.info(
        "channel_auto_selected",
        interface=interface,
        band=band,
        channel=best.channel_number,
        frequency_mhz=best.frequency_mhz,
        candidates_in_band=len(filtered),
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
