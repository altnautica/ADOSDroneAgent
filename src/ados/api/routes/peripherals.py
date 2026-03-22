"""Peripheral management routes — detects USB devices, cameras, modems."""

from __future__ import annotations

from fastapi import APIRouter

router = APIRouter()


@router.get("/peripherals")
async def list_peripherals():
    """List detected peripherals (USB devices, cameras, modems)."""
    return _scan_all()


@router.post("/peripherals/scan")
async def scan_peripherals():
    """Re-scan for connected peripherals."""
    return _scan_all()


def _scan_all() -> list[dict]:
    """Scan all peripheral buses and return unified list."""
    peripherals: list[dict] = []

    # USB devices
    try:
        from ados.hal.usb import discover_usb_devices
        for dev in discover_usb_devices():
            peripherals.append({
                "name": getattr(dev, "product", None) or f"USB {dev.vendor_id:04x}:{dev.product_id:04x}",
                "type": "usb",
                "category": _classify_usb(dev.vendor_id, dev.product_id),
                "bus": f"usb:{getattr(dev, 'bus', '?')}:{getattr(dev, 'address', '?')}",
                "address": f"{dev.vendor_id:04x}:{dev.product_id:04x}",
                "rate_hz": 0,
                "status": "ok",
                "last_reading": "",
            })
    except Exception:
        pass

    # Cameras
    try:
        from ados.hal.camera import discover_cameras
        for cam in discover_cameras():
            peripherals.append({
                "name": getattr(cam, "name", "Camera"),
                "type": getattr(cam, "interface", "unknown"),
                "category": "camera",
                "bus": getattr(cam, "device_path", ""),
                "address": getattr(cam, "device_path", ""),
                "rate_hz": 0,
                "status": "ok",
                "last_reading": "",
            })
    except Exception:
        pass

    # Cellular modems
    try:
        from ados.hal.modem import Modem
        modem = Modem()
        if modem.detected:
            peripherals.append({
                "name": getattr(modem, "model", "4G Modem"),
                "type": "cellular",
                "category": "compute",
                "bus": "usb",
                "address": getattr(modem, "device_path", ""),
                "rate_hz": 0,
                "status": "ok",
                "last_reading": f"Signal: {getattr(modem, 'signal_strength', 'N/A')}",
            })
    except Exception:
        pass

    return peripherals


def _classify_usb(vid: int, pid: int) -> str:
    """Classify USB device by VID:PID."""
    # RTL8812EU WiFi adapters (WFB-ng compatible)
    wfb_ids = {(0x0BDA, 0xA81A), (0x0BDA, 0x8812), (0x0BDA, 0x881A)}
    if (vid, pid) in wfb_ids:
        return "video"  # WFB video adapter

    # Serial port adapters (FC connection)
    serial_vids = {0x0403, 0x10C4, 0x1A86, 0x2341}  # FTDI, CP210x, CH340, Arduino
    if vid in serial_vids:
        return "sensor"

    return "compute"
