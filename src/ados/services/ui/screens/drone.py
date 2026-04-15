"""Drone status screen: paired drone id, FC mode, battery, GPS sats."""

from __future__ import annotations

from typing import Any


def render(draw: Any, width: int, height: int, state: dict) -> None:
    drone = state.get("drone") or {}
    device = drone.get("device_id") or "--"
    mode = drone.get("fc_mode") or "--"
    battery = drone.get("battery_pct")
    sats = drone.get("gps_sats")

    short_id = device
    if isinstance(device, str) and len(device) > 14:
        short_id = device[-10:]

    draw.text((0, 0), "DRONE", fill="white")
    draw.text((0, 14), str(short_id), fill="white")
    draw.text((0, 30), f"mode {mode}", fill="white")

    bat_str = f"{battery}%" if battery is not None else "--%"
    sats_str = sats if sats is not None else "--"
    draw.text((0, 46), f"bat {bat_str}  sats {sats_str}", fill="white")
