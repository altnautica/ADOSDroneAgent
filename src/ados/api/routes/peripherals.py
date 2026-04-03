"""Peripheral management routes — detects USB devices, cameras, modems."""

from __future__ import annotations

from fastapi import APIRouter

from ados.core.logging import get_logger

log = get_logger("api.peripherals")

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
                "name": dev.name or f"USB {dev.vid:04x}:{dev.pid:04x}",
                "type": "usb",
                "category": _classify_usb(dev.vid, dev.pid),
                "bus": f"usb:{dev.bus}:{dev.device}",
                "address": f"{dev.vid:04x}:{dev.pid:04x}",
                "rate_hz": 0,
                "status": "ok",
                "last_reading": "",
            })
    except Exception as e:
        log.warning("usb_scan_failed", error=str(e))

    # Cameras
    try:
        from ados.hal.camera import discover_cameras
        for cam in discover_cameras():
            peripherals.append({
                "name": getattr(cam, "name", "Camera"),
                "type": cam.type.value,
                "category": "camera",
                "bus": getattr(cam, "device_path", ""),
                "address": getattr(cam, "device_path", ""),
                "rate_hz": 0,
                "status": "ok",
                "last_reading": "",
            })
    except Exception as e:
        log.warning("camera_scan_failed", error=str(e))

    # Cellular modems
    try:
        from ados.hal.modem import detect_modem
        modem_info = detect_modem()
        if modem_info:
            peripherals.append({
                "name": modem_info.name,
                "type": "cellular",
                "category": "compute",
                "bus": "usb",
                "address": modem_info.ip_address or "",
                "rate_hz": 0,
                "status": "ok",
                "last_reading": f"Signal: {modem_info.signal_strength}% | {modem_info.operator} | {modem_info.connection_state}",
            })
    except Exception as e:
        log.warning("modem_scan_failed", error=str(e))

    return peripherals


def _classify_usb(vid: int, pid: int) -> str:
    """Classify USB device by VID:PID."""
    from ados.hal.usb import categorize_device, UsbCategory
    _, category = categorize_device(vid, pid, "")
    category_map = {
        UsbCategory.FC: "sensor",
        UsbCategory.CAMERA: "camera",
        UsbCategory.RADIO: "video",
        UsbCategory.GPS: "sensor",
        UsbCategory.LORA: "video",
        UsbCategory.OTHER: "compute",
    }
    return category_map.get(category, "compute")
