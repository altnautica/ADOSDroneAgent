"""Link status screen: RSSI, bitrate, FEC counts, channel."""

from __future__ import annotations

from typing import Any


def render(draw: Any, width: int, height: int, state: dict) -> None:
    link = state.get("link") or {}
    rssi = link.get("rssi_dbm")
    bitrate = link.get("bitrate_mbps")
    fec_rec = link.get("fec_recovered")
    fec_lost = link.get("fec_lost")
    channel = link.get("channel")

    # Header.
    draw.text((0, 0), "LINK", fill="white")
    draw.text((width - 40, 0), f"ch {channel if channel is not None else '--'}", fill="white")

    # Big RSSI.
    rssi_str = f"{rssi} dBm" if rssi is not None else "-- dBm"
    draw.text((0, 14), rssi_str, fill="white")

    br_str = f"{bitrate} Mbps" if bitrate is not None else "-- Mbps"
    draw.text((0, 30), br_str, fill="white")

    rec_str = fec_rec if fec_rec is not None else "--"
    lost_str = fec_lost if fec_lost is not None else "--"
    draw.text((0, 46), f"FEC R {rec_str}  L {lost_str}", fill="white")
