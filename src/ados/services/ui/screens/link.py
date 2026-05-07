"""Link status screen: RSSI, bitrate, FEC counts, channel, TX power."""

from __future__ import annotations

from typing import Any

# Mapping from the WfbConfig topology enum to the four-char OLED tag.
# Anything unrecognised renders as ``--`` so the line still aligns.
_TOPOLOGY_SHORT: dict[str, str] = {
    "host_vbus": "VBUS",
    "powered_hub": "HUB",
    "external_5v": "EXT",
}


def render(draw: Any, width: int, height: int, state: dict) -> None:
    link = state.get("link") or {}
    radio = state.get("radio") or {}
    rssi = link.get("rssi_dbm")
    bitrate = link.get("bitrate_mbps")
    fec_rec = link.get("fec_recovered")
    fec_lost = link.get("fec_lost")
    channel = link.get("channel")
    tx_power_dbm = link.get("tx_power_dbm")
    topology_short = _TOPOLOGY_SHORT.get((radio.get("topology") or "").lower(), "--")

    # Header.
    draw.text((0, 0), "LINK", fill="white")
    draw.text((width - 40, 0), f"ch {channel if channel is not None else '--'}", fill="white")

    # Big RSSI.
    rssi_str = f"{rssi} dBm" if rssi is not None else "-- dBm"
    draw.text((0, 13), rssi_str, fill="white")

    br_str = f"{bitrate} Mbps" if bitrate is not None else "-- Mbps"
    draw.text((0, 26), br_str, fill="white")

    rec_str = fec_rec if fec_rec is not None else "--"
    lost_str = fec_lost if fec_lost is not None else "--"
    draw.text((0, 39), f"FEC R {rec_str}  L {lost_str}", fill="white")

    # TX power + topology summary on the bottom row. Plain ASCII so
    # the bitmap fallback font on barebones systems still renders the
    # whole string.
    tx_str = f"{int(tx_power_dbm)}dBm" if tx_power_dbm is not None else "--dBm"
    draw.text((0, 52), f"TX {tx_str} {topology_short}", fill="white")
