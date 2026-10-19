"""Visor-specific probes for the boot-time profile detector.

A visor is a handheld capture rig: cameras plus a USB sensor hub, no
long-range radio. Its distinguishing signals are the ADOS Sensor Hub
firmware on USB, or a stock ArduPilot board on USB alongside two or more
CSI cameras. Kept apart from ``_profile_detect_probes`` so each module
stays small; re-exported from ``ados.bootstrap.profile_detect``.
"""

from __future__ import annotations

import subprocess

# ADOS Sensor Hub firmware USB CDC identity (pid.codes).
_SENSOR_HUB_ID = (0x1209, 0xAD05)
# Stock ArduPilot USB identity (pid.codes). Also a drone flight controller,
# so it counts toward visor only on a multi-camera rig.
_ARDUPILOT_USB_ID = (0x1209, 0x5741)

# The sensor hub firmware also enumerates as a MAVLink serial device and can
# publish a heartbeat, which earns up to 9 air points (6 serial + 3
# heartbeat). The visor weight sits above that so the firmware identity wins.
SENSOR_HUB_VISOR_POINTS = 10
ARDUPILOT_MULTICAM_VISOR_POINTS = 2
MIN_VISOR_CSI_CAMERAS = 2


def _lsusb_text() -> str:
    """Lower-cased ``lsusb`` output, or ``""`` when it cannot be read."""
    try:
        result = subprocess.run(
            ["lsusb"],
            capture_output=True,
            text=True,
            timeout=3,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return ""
    if result.returncode != 0:
        return ""
    return result.stdout.lower()


def _usb_id_present(text: str, usb_id: tuple[int, int]) -> bool:
    vid, pid = usb_id
    return f"{vid:04x}:{pid:04x}" in text


def probe_csi_cameras() -> int:
    """Number of CSI cameras the camera stack reports (0 on any failure)."""
    try:
        from ados.hal.camera import _discover_csi_cameras

        return len(_discover_csi_cameras())
    except Exception:
        return 0


def probe_sensor_hub() -> tuple[int, bool]:
    """Score the USB sensor hub signal toward the visor profile.

    Returns ``(visor_points, detected)``. The ADOS Sensor Hub firmware
    (``1209:ad05``) is a strong visor signal. A stock ArduPilot board
    (``1209:5741``) earns a small visor score only when at least
    ``MIN_VISOR_CSI_CAMERAS`` CSI cameras are present; the camera probe
    runs only in that case.
    """
    text = _lsusb_text()
    if not text:
        return 0, False
    if _usb_id_present(text, _SENSOR_HUB_ID):
        return SENSOR_HUB_VISOR_POINTS, True
    if _usb_id_present(text, _ARDUPILOT_USB_ID):
        if probe_csi_cameras() >= MIN_VISOR_CSI_CAMERAS:
            return ARDUPILOT_MULTICAM_VISOR_POINTS, True
    return 0, False
