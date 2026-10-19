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

from collections.abc import Iterable
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from ados.core.logging import get_logger
from ados.setup._hw_probes import (  # noqa: F401 - re-exported at the old path
    _FIRMWARE_NAMES,
    _check_4g,
    _check_board,
    _check_buttons,
    _check_camera,
    _check_fc,
    _check_gps,
    _check_hdmi,
    _check_joystick,
    _check_mesh_dongle,
    _check_oled,
    _check_radio_wfb,
    _check_uplink,
    _fc_firmware_name,
    _fc_label,
    _radio_devices,
)
from ados.setup.models import HardwareCheckItem, HardwareCheckStatus

# Kernel driver names exported under /sys/class/graphics/fbN/name when an
# SPI LCD is bound. Used by _check_display to identify a framebuffer
# regardless of whether it landed at /dev/fb0 or /dev/fb1 (the index
# depends on whether the primary display driver is also active).
_SPI_LCD_DRIVER_NAMES = frozenset(
    {
        "fb_ili9486",
        "fb_ili9341",
        "fb_ili9340",
        "fb_st7789v",
        "fb_st7735r",
        "fb_hx8347d",
        "fb_hx8353d",
        "fb_pcd8544",
        "fb_ssd1351",
    }
)

log = get_logger("setup.hardware_check")


def _now_iso() -> str:
    return (
        datetime.now(timezone.utc).astimezone().replace(microsecond=0).isoformat()
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
    expected_name = conf.get("framebuffer_name_expected") or "fb_ili9486"
    configured_path = conf.get("framebuffer_path", "")

    has_touch = (conf.get("has_touch", "false").lower() == "true")
    res = conf.get("resolution", "?")
    rotation = conf.get("rotation", "0")
    suffix = " + touch" if has_touch else ""

    # Walk all /dev/fb* and pick the framebuffer that's actually bound
    # to a known SPI-LCD driver. Hard-coding /dev/fb1 breaks on Pi 4B
    # without an HDMI monitor: bcm2708_fb disables itself, the SPI
    # driver claims /dev/fb0 instead, and the probe falsely reports
    # "fb1 not bound" even though the panel is fully functional.
    matched_fb_path: Path | None = None
    matched_fb_name = ""
    for entry in sorted(Path("/dev").glob("fb*")):
        # Skip non-fb files in /dev (e.g. fbdev*) — only /dev/fbN where
        # N is a digit.
        if not entry.name.startswith("fb") or not entry.name[2:].isdigit():
            continue
        # Build the path as a single string so a test that patches
        # Path("...") at the module level can intercept the lookup. A
        # chained `Path("/sys/class/graphics") / entry.name / "name"`
        # only goes through the patched constructor once and the rest
        # of the chain stays on the real filesystem.
        name_path = Path(f"/sys/class/graphics/{entry.name}/name")
        try:
            fb_name = name_path.read_text().strip()
        except OSError:
            continue
        if expected_name and expected_name in fb_name:
            matched_fb_path = entry
            matched_fb_name = fb_name
            break
        if fb_name in _SPI_LCD_DRIVER_NAMES:
            matched_fb_path = entry
            matched_fb_name = fb_name
            # Don't break: prefer the configured expected_name match
            # over a generic SPI-LCD match. Only use this as fallback.

    if matched_fb_path is None:
        # No SPI LCD framebuffer found. If display.conf has a configured
        # path, mention it in the diagnostic so the operator can confirm
        # the overlay is staged correctly.
        hint_path = configured_path or "/dev/fb*"
        return HardwareCheckItem(
            id="display",
            label="Local display (SPI LCD)",
            state="warning",
            detail=(
                f"{display_id} provisioned but no SPI LCD framebuffer is bound "
                f"({hint_path}). Reboot to load the overlay if the install "
                "just ran."
            ),
            fix_hint="Run `sudo reboot`. The kernel binds the panel at boot.",
        )

    return HardwareCheckItem(
        id="display",
        label="Local display (SPI LCD)",
        state="ok",
        detail=(
            f"{display_id} on {matched_fb_path} ({matched_fb_name}, "
            f"{res}, rotation {rotation}{suffix})."
        ),
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


def _visor_items(runtime: Any) -> Iterable[HardwareCheckItem]:
    # Handheld rig: cameras, USB sensor hub, GNSS, local display and buttons.
    # No WFB radio.
    yield _check_board()
    yield _check_fc(runtime)
    yield _check_camera()
    yield _check_gps()
    yield _check_oled()
    yield _check_display()
    yield _check_buttons()
    yield _check_uplink(runtime)


_CHECKED_PROFILES = ("drone", "ground_station", "visor")


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
    profile_norm = profile if profile in _CHECKED_PROFILES else "drone"
    role_norm = (ground_role or "direct") if profile_norm == "ground_station" else ""

    if profile_norm == "drone":
        items = list(_drone_items(runtime))
    elif profile_norm == "visor":
        items = list(_visor_items(runtime))
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

    profile_norm = profile if profile in _CHECKED_PROFILES else "drone"
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
