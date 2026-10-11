"""Per-component hardware probes for the onboarding hardware check.

Each probe composes an existing HAL or bootstrap probe into one
``HardwareCheckItem``. The display probe and the orchestrator live in
``ados.setup.hardware_check``.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from ados.core.logging import get_logger
from ados.setup.models import HardwareCheckItem

log = get_logger("setup.hardware_check")


# ---- Per-component probes (synchronous orchestration over the HAL) -------


def _check_board() -> HardwareCheckItem:
    try:
        from ados.hal.detect import detect_board

        info = detect_board()
        detail = (
            f"{info.name} ({info.model}). "
            f"{info.cpu_cores} cores, {info.ram_mb} MB RAM, tier {info.tier}."
        )
        return HardwareCheckItem(
            id="board",
            label="Companion compute",
            required=True,
            state="ok",
            detail=detail,
        )
    except Exception as exc:  # pragma: no cover - HAL failure is non-fatal
        return HardwareCheckItem(
            id="board",
            label="Companion compute",
            required=True,
            state="warning",
            detail=f"Could not fingerprint board: {exc}",
            fix_hint="Check that /proc/device-tree/model is readable.",
        )


_FIRMWARE_NAMES = {
    "px4": "PX4",
    "betaflight": "Betaflight",
    "inav": "iNav",
}


def _fc_firmware_name(token: str | None) -> str | None:
    """Human FC firmware name from a family/variant token, or None."""
    if not token:
        return None
    t = str(token).strip().lower()
    if t.startswith("ardupilot"):
        return "ArduPilot"
    return _FIRMWARE_NAMES.get(t)


def _fc_label(firmware: str | None, variant: str | None) -> str:
    """A firmware-neutral flight-controller label, e.g. "Flight controller ·
    Betaflight (MSP)" or "Flight controller · ArduPilot"."""
    name = _fc_firmware_name(firmware) or _fc_firmware_name(variant)
    if not name:
        return "Flight controller"
    is_msp = bool(variant) or firmware in ("betaflight", "inav")
    return f"Flight controller · {name}{' (MSP)' if is_msp else ''}"


def _check_fc(runtime: Any) -> HardwareCheckItem:
    """Flight controller presence + link/reachability read (firmware-neutral)."""
    fc = runtime.fc_status() if runtime else None
    connected = bool(getattr(fc, "connected", False))
    reachable = bool(getattr(fc, "reachable", False))
    variant = getattr(fc, "fc_variant", None) if fc else None
    firmware = getattr(fc, "fc_firmware", None) if fc else None
    port = str(getattr(fc, "port", "") or "") if fc else ""
    baud = getattr(fc, "baud", None) if fc else None

    if connected or reachable:
        # A connected MAVLink FC (ArduPilot/PX4) OR a reachable MSP FC
        # (Betaflight/iNav), which is drivable over the MSP proxy even though it
        # never emits a MAVLink heartbeat. Both are a healthy flight controller.
        bits = ["MSP" if variant else "MAVLink"]
        if port:
            bits.append(f"on {port}")
        if baud:
            bits.append(f"@ {baud} baud")
        return HardwareCheckItem(
            id="fc",
            label=_fc_label(firmware, variant),
            required=True,
            state="ok",
            detail=" ".join(bits),
        )

    # Fall back to serial-presence probe so the operator at least sees that
    # a candidate device exists even before the link is up.
    from ados.bootstrap.profile_detect import probe_mavlink_serial

    _g, _a, has_serial = probe_mavlink_serial()
    if has_serial:
        return HardwareCheckItem(
            id="fc",
            label="Flight controller",
            required=True,
            state="warning",
            detail="Serial device present but the flight controller is not responding yet.",
            fix_hint="Confirm the FC is powered and the baud rate is correct.",
        )
    return HardwareCheckItem(
        id="fc",
        label="Flight controller",
        required=True,
        state="missing",
        detail="No flight controller serial device detected.",
        fix_hint="Connect the FC USB cable to the companion and re-check.",
    )


def _check_camera() -> HardwareCheckItem:
    try:
        from ados.hal.camera import discover_cameras

        cameras = discover_cameras()
    except Exception as exc:
        log.warning("camera_probe_failed", error=str(exc))
        return HardwareCheckItem(
            id="camera",
            label="Camera",
            required=True,
            state="warning",
            detail=f"Camera enumeration failed: {exc}",
            fix_hint="Verify v4l2 / rpicam tools are installed.",
        )

    if not cameras:
        return HardwareCheckItem(
            id="camera",
            label="Camera",
            required=True,
            state="missing",
            detail="No CSI or USB camera detected.",
            fix_hint="Plug in a USB UVC camera or attach a MIPI CSI module.",
        )

    by_type: dict[str, int] = {}
    for cam in cameras:
        key = (
            cam.type.value if hasattr(cam.type, "value") else str(cam.type)
        )
        by_type[key] = by_type.get(key, 0) + 1
    summary = ", ".join(f"{count} {kind}" for kind, count in sorted(by_type.items()))
    primary = cameras[0]
    primary_path = getattr(primary, "device_path", "")
    return HardwareCheckItem(
        id="camera",
        label="Camera",
        required=True,
        state="ok",
        detail=(
            f"{len(cameras)} detected ({summary}). Primary: {primary.name}"
            + (f" at {primary_path}" if primary_path else "")
        ),
    )


def _radio_devices() -> list[Any]:
    try:
        from ados.hal.usb import UsbCategory, discover_usb_devices

        return [d for d in discover_usb_devices() if d.category == UsbCategory.RADIO]
    except Exception:
        return []


def _check_radio_wfb(required: bool = False, min_count: int = 1) -> HardwareCheckItem:
    devices = _radio_devices()
    count = len(devices)
    if count >= min_count:
        names = ", ".join(sorted({d.description or d.name for d in devices}))
        state = "ok"
        detail = f"{count} adapter(s) detected: {names}"
    elif count > 0:
        state = "warning"
        detail = f"{count} adapter(s) detected, profile expects at least {min_count}."
    else:
        state = "missing" if required else "warning"
        detail = "No RTL8812-class WFB adapter detected on USB."
    return HardwareCheckItem(
        id="radio_wfb",
        label="WFB radio adapter",
        required=required,
        state=state,
        detail=detail,
        fix_hint=(
            "Plug in an RTL8812EU/AU USB adapter."
            if state != "ok"
            else ""
        ),
    )


def _check_mesh_dongle(required: bool) -> HardwareCheckItem:
    if len(_radio_devices()) >= 2:
        return HardwareCheckItem(
            id="mesh_dongle",
            label="Mesh second-radio dongle",
            required=required,
            state="ok",
            detail="Two RTL8812-class WFB adapters detected. Eligible for batman-adv mesh.",
        )
    return HardwareCheckItem(
        id="mesh_dongle",
        label="Mesh second-radio dongle",
        required=required,
        state="missing" if required else "warning",
        detail="Fewer than two RTL8812-class WFB adapters detected.",
        fix_hint="Mesh roles need two RTL8812-class USB adapters.",
    )


def _check_oled() -> HardwareCheckItem:
    try:
        from ados.bootstrap.profile_detect import probe_i2c_oled

        _g, _a, detected = probe_i2c_oled()
    except Exception:
        detected = False
    if detected:
        return HardwareCheckItem(
            id="oled",
            label="OLED display",
            state="ok",
            detail="SSD1306/SH1106 detected on I2C bus 1.",
        )
    return HardwareCheckItem(
        id="oled",
        label="OLED display",
        state="warning",
        detail="No OLED detected at I2C 0x3C/0x3D.",
        fix_hint="Optional. Status webapp still works without it.",
    )


def _check_buttons() -> HardwareCheckItem:
    try:
        from ados.bootstrap.profile_detect import board_button_pins

        pins = board_button_pins()
    except Exception:
        pins = []
    if pins:
        pin_list = ", ".join(str(pin) for pin in pins)
        return HardwareCheckItem(
            id="buttons",
            label="Front-panel buttons",
            state="unknown",
            detail=(
                f"This board declares front-panel buttons on BCM pins {pin_list}. "
                "Whether buttons are wired to them cannot be checked."
            ),
            fix_hint="Optional. Press each button and watch the OLED menu respond.",
        )
    return HardwareCheckItem(
        id="buttons",
        label="Front-panel buttons",
        state="unknown",
        detail="This board declares no front-panel buttons.",
        fix_hint="Optional. The status webapp works without them.",
    )


def _check_joystick() -> HardwareCheckItem:
    """Best-effort joystick presence via /dev/input/js*.

    The async InputManager API is the canonical surface for live use; for
    a one-shot wizard probe a direct filesystem scan is enough.
    """
    paths = sorted(Path("/dev/input").glob("js*")) if Path("/dev/input").is_dir() else []
    if paths:
        return HardwareCheckItem(
            id="joystick",
            label="Joystick / gamepad",
            state="ok",
            detail=f"{len(paths)} device(s) at {', '.join(str(p) for p in paths)}.",
        )
    return HardwareCheckItem(
        id="joystick",
        label="Joystick / gamepad",
        state="warning",
        detail="No /dev/input/js* devices detected.",
        fix_hint="Optional. Plug in a USB gamepad or RC controller.",
    )


def _check_hdmi() -> HardwareCheckItem:
    """Best-effort HDMI hot-plug detection via /sys/class/drm.

    Real DRM enumeration is out of scope for v1; we only check the kernel
    sysfs status flag for any HDMI-* connector that exposes one.
    """
    drm = Path("/sys/class/drm")
    if not drm.is_dir():
        return HardwareCheckItem(
            id="hdmi",
            label="HDMI output",
            state="unknown",
            detail="DRM sysfs not available on this platform.",
        )
    connected: list[str] = []
    seen_any = False
    for entry in drm.iterdir():
        if "HDMI" not in entry.name.upper():
            continue
        seen_any = True
        status = entry / "status"
        try:
            value = status.read_text().strip().lower()
        except OSError:
            continue
        if value == "connected":
            connected.append(entry.name)
    if connected:
        return HardwareCheckItem(
            id="hdmi",
            label="HDMI output",
            state="ok",
            detail=f"Connected on {', '.join(connected)}.",
        )
    if seen_any:
        return HardwareCheckItem(
            id="hdmi",
            label="HDMI output",
            state="warning",
            detail="HDMI port present but no display connected.",
            fix_hint="Optional. Plug in an HDMI display for the kiosk view.",
        )
    return HardwareCheckItem(
        id="hdmi",
        label="HDMI output",
        state="unknown",
        detail="No HDMI connector exposed by the kernel on this board.",
    )


def _check_uplink(runtime: Any) -> HardwareCheckItem:
    """Any-uplink-up summary across ethernet / WiFi / USB tether / 4G."""
    from ados.bootstrap.profile_detect import probe_uplink_kinds

    detected_kinds: list[str] = []
    try:
        detected_kinds.extend(probe_uplink_kinds())
    except Exception as exc:
        log.warning("uplink_probe_failed", error=str(exc))

    try:
        from ados.hal.modem import detect_modem

        modem = detect_modem()
        if modem and modem.connection_state.lower() in ("connected", "registered"):
            detected_kinds.append("4G")
    except Exception as exc:
        log.warning("modem_probe_failed", error=str(exc))

    if detected_kinds:
        return HardwareCheckItem(
            id="uplink",
            label="Uplink to internet",
            required=False,
            state="ok",
            detail=f"Active via {', '.join(detected_kinds)}.",
        )
    return HardwareCheckItem(
        id="uplink",
        label="Uplink to internet",
        required=False,
        state="warning",
        detail="No active uplink detected (ethernet, WiFi, USB tether, or 4G).",
        fix_hint=(
            "Optional. Cloud relay and Mission Control pairing need an uplink."
        ),
    )


def _check_4g() -> HardwareCheckItem:
    try:
        from ados.hal.modem import detect_modem

        modem = detect_modem()
    except Exception:
        modem = None
    if modem is None:
        return HardwareCheckItem(
            id="radio_4g",
            label="4G LTE modem",
            state="missing",
            detail="No cellular modem detected by ModemManager.",
            fix_hint="Optional. Plug in a USB LTE modem if you need cellular fallback.",
        )
    if modem.connection_state.lower() in ("connected", "registered"):
        operator = modem.operator or "carrier"
        return HardwareCheckItem(
            id="radio_4g",
            label="4G LTE modem",
            state="ok",
            detail=f"{modem.name} on {operator}, signal {modem.signal_strength}%.",
        )
    return HardwareCheckItem(
        id="radio_4g",
        label="4G LTE modem",
        state="warning",
        detail=f"{modem.name} present but {modem.connection_state}.",
        fix_hint="Check SIM, APN, and signal.",
    )


def _check_gps() -> HardwareCheckItem:
    try:
        from ados.bootstrap.profile_detect import probe_gps_serial

        _g, _a, detected = probe_gps_serial()
    except Exception:
        detected = False
    if detected:
        return HardwareCheckItem(
            id="gps",
            label="GPS receiver",
            state="ok",
            detail="GPS serial device detected.",
        )
    return HardwareCheckItem(
        id="gps",
        label="GPS receiver",
        state="warning",
        detail="GPS auto-detection is best-effort. The FC reports GPS fix/sats over its own telemetry once connected.",
    )
