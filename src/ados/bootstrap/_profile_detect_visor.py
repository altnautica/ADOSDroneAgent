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
# so it marks a visor only on a multi-camera rig with no long-range radio
# (the detector applies the radio rule).
_ARDUPILOT_USB_ID = (0x1209, 0x5741)
MIN_VISOR_CSI_CAMERAS = 2

# Hub signal kinds returned by ``probe_sensor_hub``.
HUB_FIRMWARE = "sensor_hub"
HUB_ARDUPILOT_MULTICAM = "ardupilot_multicam"


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


def probe_sensor_hub() -> str:
    """Classify the USB sensor-hub signal.

    Returns ``HUB_FIRMWARE`` when the ADOS Sensor Hub firmware
    (``1209:ad05``) is on USB, ``HUB_ARDUPILOT_MULTICAM`` when a stock
    ArduPilot board (``1209:5741``) sits next to at least
    ``MIN_VISOR_CSI_CAMERAS`` CSI cameras (the camera probe runs only then),
    and ``""`` otherwise.
    """
    text = _lsusb_text()
    if not text:
        return ""
    if _usb_id_present(text, _SENSOR_HUB_ID):
        return HUB_FIRMWARE
    if _usb_id_present(text, _ARDUPILOT_USB_ID):
        if probe_csi_cameras() >= MIN_VISOR_CSI_CAMERAS:
            return HUB_ARDUPILOT_MULTICAM
    return ""
