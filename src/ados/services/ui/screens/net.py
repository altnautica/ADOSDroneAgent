"""Network screen: AP SSID, AP IP, USB IP, uplink type, uplink status."""

from __future__ import annotations

from typing import Any


def render(draw: Any, width: int, height: int, state: dict) -> None:
    net = state.get("network") or {}
    ap_ssid = net.get("ap_ssid") or "--"
    ap_ip = net.get("ap_ip") or "--"
    usb_ip = net.get("usb_ip")
    uplink = net.get("uplink_type") or "none"
    uplink_ok = net.get("uplink_reachable")

    draw.text((0, 0), "NET", fill="white")
    draw.text((0, 14), f"AP {ap_ssid}"[:21], fill="white")
    draw.text((0, 30), f"{ap_ip}", fill="white")

    if usb_ip:
        draw.text((0, 46), f"usb {usb_ip}  up {uplink}"[:21], fill="white")
    else:
        status = "OK" if uplink_ok else ("DOWN" if uplink != "none" else "--")
        draw.text((0, 46), f"up {uplink} {status}"[:21], fill="white")
