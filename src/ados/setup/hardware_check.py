"""Profile-aware hardware presence + readiness orchestrator.

Composes the existing HAL probes (`ados.hal.detect`, `ados.hal.usb`,
`ados.hal.camera`, `ados.hal.modem`) and the bootstrap profile probes
(`ados.bootstrap.profile_detect`) into a single per-component readout
the onboarding wizard surfaces as the hardware-check step.

Each component returns one ``HardwareCheckItem`` so the wizard renders a
flat list of rows with green/red/yellow badges. No new probes are
introduced here; this module is pure orchestration.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

from ados.core.logging import get_logger
from ados.setup.models import HardwareCheckItem, HardwareCheckStatus

log = get_logger("setup.hardware_check")


def _now_iso() -> str:
    return (
        datetime.now(timezone.utc).astimezone().replace(microsecond=0).isoformat()
    )


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


def _check_fc(runtime: Any) -> HardwareCheckItem:
    """Flight controller presence + (best-effort) heartbeat read."""
    fc = runtime.fc_status() if runtime else None
    connected = bool(getattr(fc, "connected", False))
    if connected:
        port = str(getattr(fc, "port", "") or "")
        baud = getattr(fc, "baud", None)
        autopilot = getattr(fc, "autopilot_type", "") or "MAVLink"
        version = getattr(fc, "firmware_version", "") or ""
        bits = [autopilot]
        if version:
            bits.append(str(version))
        if port:
            bits.append(f"on {port}")
        if baud:
            bits.append(f"@ {baud} baud")
        return HardwareCheckItem(
            id="fc",
            label="Flight controller (MAVLink)",
            required=True,
            state="ok",
            detail=" ".join(bits),
        )

    # Fall back to serial-presence probe so the operator at least sees that
    # a candidate device exists even before MAVLink lock.
    from ados.bootstrap.profile_detect import probe_mavlink_serial

    _g, _a, has_serial = probe_mavlink_serial()
    if has_serial:
        return HardwareCheckItem(
            id="fc",
            label="Flight controller (MAVLink)",
            required=True,
            state="warning",
            detail="Serial device present but MAVLink heartbeat not yet detected.",
            fix_hint="Confirm baud rate and that the FC is powered.",
        )
    return HardwareCheckItem(
        id="fc",
        label="Flight controller (MAVLink)",
        required=True,
        state="missing",
        detail="No flight controller serial device detected.",
        fix_hint="Connect FC USB cable to the companion and re-check.",
    )


def _check_camera() -> HardwareCheckItem:
    try:
        from ados.hal.camera import CameraType, discover_cameras

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
    try:
        from ados.bootstrap.profile_detect import probe_mesh_capable

        capable = probe_mesh_capable()
    except Exception:
        capable = False
    if capable:
        return HardwareCheckItem(
            id="mesh_dongle",
            label="Mesh second-radio dongle",
            required=required,
            state="ok",
            detail="Second USB wireless adapter detected. Eligible for batman-adv mesh.",
        )
    return HardwareCheckItem(
        id="mesh_dongle",
        label="Mesh second-radio dongle",
        required=required,
        state="missing" if required else "warning",
        detail="No second USB wireless adapter detected.",
        fix_hint="Mesh roles need a second USB WiFi adapter for batman-adv carrier.",
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


def _check_display() -> HardwareCheckItem:
    """SPI LCD readiness probe.

    Returns one of four states based on what we can read from
    ``/etc/ados/display.conf`` (written by the LCD-overlay installer)
    and ``/sys/class/graphics/fb1/name`` (populated by the kernel
    once the device-tree overlay binds the panel after a reboot):

    * ``ok``: conf present AND fb1 reports the expected driver name.
    * ``warning`` (pending_reboot): conf present but fb1 absent or
      bound to a different driver — the operator has run the install
      step but not yet rebooted.
    * ``unknown`` (not_configured): no conf file at all; the board may
      not have any displays.supported entry, or the operator passed
      ``--display none``.
    * ``warning`` (error): conf present but the running kernel
      framebuffer disagrees with the expected driver name.
    """
    from ados.core.paths import DISPLAY_CONF_PATH

    if not DISPLAY_CONF_PATH.exists():
        return HardwareCheckItem(
            id="display",
            label="Local display (SPI LCD)",
            state="unknown",
            detail="No /etc/ados/display.conf — no LCD provisioned for this board.",
            fix_hint=(
                "Optional. Plug a supported SPI LCD and rerun the install "
                "with --upgrade to provision the device-tree overlay."
            ),
        )

    conf: dict[str, str] = {}
    try:
        for raw in DISPLAY_CONF_PATH.read_text().splitlines():
            line = raw.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, _, v = line.partition("=")
            conf[k.strip()] = v.strip()
    except OSError as exc:
        return HardwareCheckItem(
            id="display",
            label="Local display (SPI LCD)",
            state="warning",
            detail=f"display.conf unreadable: {exc}",
        )

    display_id = conf.get("display_id", "?")
    fb_path = Path(conf.get("framebuffer_path", "/dev/fb1"))
    expected_name = conf.get("framebuffer_name_expected") or "fb_ili9486"

    if not fb_path.exists():
        return HardwareCheckItem(
            id="display",
            label="Local display (SPI LCD)",
            state="warning",
            detail=(
                f"{display_id} provisioned but {fb_path} is not bound. "
                "Reboot to load the overlay."
            ),
            fix_hint="Run `sudo reboot`. The kernel binds the panel at boot.",
        )

    fb_name_path = Path("/sys/class/graphics") / fb_path.name / "name"
    actual_name = ""
    try:
        actual_name = fb_name_path.read_text().strip()
    except OSError:
        pass

    has_touch = (conf.get("has_touch", "false").lower() == "true")
    res = conf.get("resolution", "?")
    rotation = conf.get("rotation", "0")
    suffix = " + touch" if has_touch else ""

    if expected_name and actual_name and expected_name not in actual_name:
        return HardwareCheckItem(
            id="display",
            label="Local display (SPI LCD)",
            state="warning",
            detail=(
                f"{fb_path} bound to {actual_name}, expected {expected_name}. "
                "Overlay may have changed."
            ),
            fix_hint=(
                "Re-run `sudo install.sh --upgrade` to refresh the "
                "device-tree overlay, then reboot."
            ),
        )

    return HardwareCheckItem(
        id="display",
        label="Local display (SPI LCD)",
        state="ok",
        detail=(
            f"{display_id} on {fb_path} ({res}, rotation {rotation}{suffix})."
        ),
    )


def _check_buttons() -> HardwareCheckItem:
    try:
        from ados.bootstrap.profile_detect import probe_gpio_buttons

        _g, _a, detected = probe_gpio_buttons()
    except Exception:
        detected = False
    if detected:
        return HardwareCheckItem(
            id="buttons",
            label="Front-panel buttons",
            state="ok",
            detail="Four buttons read idle-high on default GPIOs.",
        )
    return HardwareCheckItem(
        id="buttons",
        label="Front-panel buttons",
        state="warning",
        detail="Buttons not detected on default GPIOs.",
        fix_hint="Optional. Wire buttons to BCM pins 5, 6, 13, 19 for OLED nav.",
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
        detail="GPS auto-detection is best-effort. Trust MAVLink GPS_RAW once FC is connected.",
    )


# ---- Public orchestrator -------------------------------------------------


def _drone_items(runtime: Any) -> Iterable[HardwareCheckItem]:
    yield _check_board()
    yield _check_fc(runtime)
    yield _check_camera()
    yield _check_uplink(runtime)
    yield _check_radio_wfb(required=False)
    yield _check_4g()
    yield _check_gps()


def _ground_items(role: str) -> Iterable[HardwareCheckItem]:
    is_mesh = role in ("relay", "receiver")
    yield _check_board()
    yield _check_radio_wfb(required=True, min_count=1)
    if is_mesh:
        yield _check_mesh_dongle(required=True)
    else:
        yield _check_mesh_dongle(required=False)
    yield _check_oled()
    yield _check_display()
    yield _check_buttons()
    yield _check_hdmi()
    yield _check_joystick()
    yield _check_uplink(runtime=None)


def run_hardware_check_fresh(
    runtime: Any | None,
    *,
    profile: str,
    ground_role: str | None = None,
) -> HardwareCheckStatus:
    """Run the full per-profile hardware-check sweep, no caching.

    The orchestrator never raises. A failed probe yields an item in the
    ``warning`` or ``unknown`` state with a human-readable hint.
    """
    profile_norm = profile if profile in ("drone", "ground_station") else "drone"
    role_norm = (ground_role or "direct") if profile_norm == "ground_station" else ""

    if profile_norm == "drone":
        items = list(_drone_items(runtime))
    else:
        items = list(_ground_items(role_norm or "direct"))

    return HardwareCheckStatus(
        profile=profile_norm,
        ground_role=role_norm,
        items=items,
        last_run=_now_iso(),
    )


def run_hardware_check_cached(
    runtime: Any | None,
    *,
    profile: str,
    ground_role: str | None = None,
    ttl_seconds: int | None = None,
) -> HardwareCheckStatus:
    """Return the cached snapshot when fresh; probe + persist otherwise.

    Used on the read path (``GET /api/v1/setup/status``,
    ``GET /api/v1/setup/hardware-check``) so the dashboard's 8 s
    polling loop doesn't re-spawn rpicam-hello / v4l2-ctl / lsusb
    on every tick. The write path
    (``POST /api/v1/setup/hardware-check/refresh``) always calls
    ``run_hardware_check_fresh`` directly.
    """
    from ados.setup import hardware_state

    profile_norm = profile if profile in ("drone", "ground_station") else "drone"
    role_norm = (ground_role or "direct") if profile_norm == "ground_station" else ""
    ttl = (
        ttl_seconds
        if ttl_seconds is not None
        else hardware_state.DEFAULT_TTL_SECONDS
    )

    cached = hardware_state.read()
    if (
        cached is not None
        and hardware_state.matches(
            cached, profile=profile_norm, ground_role=role_norm
        )
        and hardware_state.is_fresh(cached, ttl_seconds=ttl)
    ):
        return cached

    fresh = run_hardware_check_fresh(
        runtime, profile=profile_norm, ground_role=role_norm
    )
    hardware_state.write(fresh)
    return fresh


# Backwards-compat wrapper so callers that haven't been updated keep
# working. The default is now the cached path. Tests + the explicit
# refresh route call ``run_hardware_check_fresh`` directly.
def run_hardware_check(
    runtime: Any | None,
    *,
    profile: str,
    ground_role: str | None = None,
) -> HardwareCheckStatus:
    return run_hardware_check_cached(
        runtime, profile=profile, ground_role=ground_role
    )


def derive_step_state(check: HardwareCheckStatus) -> tuple[str, str]:
    """Reduce a ``HardwareCheckStatus`` to a (step_state, detail) pair.

    Used by the wizard's step assembler so the dot stepper and per-step
    summary stay in sync with the per-component readout.
    """
    required = [item for item in check.items if item.required]
    required_ok = sum(1 for item in required if item.state == "ok")
    required_total = len(required)

    if required_total == 0:
        # Profiles with no required items default to complete on first run.
        return ("complete", "Hardware check passed.")

    if required_ok == required_total:
        return (
            "complete",
            f"All {required_total} required components detected.",
        )
    return (
        "needs_action",
        f"{required_ok} of {required_total} required components detected.",
    )
