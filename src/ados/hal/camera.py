"""Camera discovery — CSI (rpicam), USB (v4l2), and IP cameras."""

from __future__ import annotations

import platform
import re
import subprocess
from dataclasses import dataclass, field
from enum import StrEnum

from ados.core.logging import get_logger

log = get_logger("hal.camera")


class CameraType(StrEnum):
    CSI = "csi"
    USB = "usb"
    IP = "ip"


class HardwareRole(StrEnum):
    CAMERA = "camera"
    CODEC = "codec"
    ISP = "isp"
    DECODER = "decoder"


@dataclass
class CameraInfo:
    """Represents a discovered camera or video hardware device."""

    name: str
    type: CameraType
    device_path: str
    width: int = 0
    height: int = 0
    capabilities: list[str] = field(default_factory=list)
    hardware_role: HardwareRole = HardwareRole.CAMERA

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "type": self.type.value,
            "device_path": self.device_path,
            "width": self.width,
            "height": self.height,
            "capabilities": self.capabilities,
            "hardware_role": self.hardware_role.value,
        }


def _discover_csi_cameras() -> list[CameraInfo]:
    """Detect CSI cameras via rpicam-hello --list-cameras."""
    cameras: list[CameraInfo] = []
    try:
        result = subprocess.run(
            ["rpicam-hello", "--list-cameras"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode != 0:
            return cameras

        # Parse output lines like: 0 : imx219 [3280x2464 10-bit RGGB] ...
        output = result.stderr + result.stdout
        cam_pattern = re.compile(r"(\d+)\s*:\s*(\S+)\s*\[(\d+)x(\d+)")
        for match in cam_pattern.finditer(output):
            idx = match.group(1)
            sensor = match.group(2)
            width = int(match.group(3))
            height = int(match.group(4))
            cameras.append(CameraInfo(
                name=f"CSI-{idx} ({sensor})",
                type=CameraType.CSI,
                device_path=f"/dev/video{idx}",
                width=width,
                height=height,
                capabilities=["h264", "mjpeg"],
            ))

        if cameras:
            log.info("csi_cameras_found", count=len(cameras))
    except FileNotFoundError:
        log.debug("rpicam_not_found", msg="rpicam-hello not in PATH")
    except subprocess.TimeoutExpired:
        log.warning("rpicam_timeout")

    return cameras


def _discover_usb_cameras() -> list[CameraInfo]:
    """Detect USB cameras via v4l2-ctl --list-devices.

    DEC-106 Bug #16: v4l2-ctl --list-devices exits non-zero when any
    /dev/videoN node fails to open (e.g. a stale node from a recently
    unplugged device), but still prints valid cameras to stdout. The old
    early-return on `returncode != 0` threw away good data. We now parse
    stdout regardless of exit code.

    DEC-106 Bug #21: UVC cameras create TWO /dev/videoN nodes per physical
    camera (main capture stream + metadata). The old parser added ALL
    listed nodes as separate CameraInfo entries, so a single USB camera
    appeared as two CameraInfo objects — camera_mgr.auto_assign() then
    picked one as primary and one as secondary, both pointing at the
    same physical device. Fix: only add the FIRST /dev/videoN in each
    device-name block (the subsequent nodes are metadata/alternates).
    """
    cameras: list[CameraInfo] = []
    try:
        result = subprocess.run(
            ["v4l2-ctl", "--list-devices"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        # DEC-106 Bug #16: parse stdout regardless of returncode

        # Parse blocks: device name on one line, /dev/videoN on next indented lines
        current_name = ""
        block_consumed = False  # DEC-106 Bug #21: one CameraInfo per device block
        for line in result.stdout.splitlines():
            stripped = line.strip()
            if not stripped:
                current_name = ""
                block_consumed = False
                continue
            if not line.startswith("\t") and not line.startswith("    "):
                # Device name line (strip trailing colon and bus info)
                current_name = stripped.rstrip(":").split("(")[0].strip()
                block_consumed = False
            elif stripped.startswith("/dev/video"):
                if block_consumed:
                    # Skip subsequent nodes in the same device block
                    # (UVC metadata node, alternate formats, etc.)
                    continue
                # Classify Pi internal hardware vs actual cameras
                name_lower = (current_name or "").lower()
                role = HardwareRole.CAMERA
                if "codec" in name_lower:
                    role = HardwareRole.CODEC
                elif "isp" in name_lower:
                    role = HardwareRole.ISP
                elif "hevc" in name_lower or "rpivid" in name_lower:
                    role = HardwareRole.DECODER
                cameras.append(CameraInfo(
                    name=current_name or "USB Camera",
                    type=CameraType.USB,
                    device_path=stripped,
                    capabilities=["mjpeg", "yuyv"] if role == HardwareRole.CAMERA else ["h264"] if role == HardwareRole.CODEC else [],
                    hardware_role=role,
                ))
                block_consumed = True

        if cameras:
            log.info("usb_cameras_found", count=len(cameras))
    except FileNotFoundError:
        log.debug("v4l2_not_found", msg="v4l2-ctl not in PATH")
    except subprocess.TimeoutExpired:
        log.warning("v4l2_timeout")

    return cameras


def _cameras_from_config(ip_sources: list[dict[str, str]]) -> list[CameraInfo]:
    """Build CameraInfo objects from config-provided IP camera entries.

    Each entry should have at minimum a ``url`` key.  An optional ``name`` key
    is used as the display name.
    """
    cameras: list[CameraInfo] = []
    for idx, entry in enumerate(ip_sources):
        url = entry.get("url", "")
        if not url:
            continue
        name = entry.get("name", f"IP-{idx}")
        cameras.append(CameraInfo(
            name=name,
            type=CameraType.IP,
            device_path=url,
            capabilities=["rtsp"],
        ))
    if cameras:
        log.info("ip_cameras_configured", count=len(cameras))
    return cameras


def discover_cameras(
    ip_sources: list[dict[str, str]] | None = None,
) -> list[CameraInfo]:
    """Discover all available cameras (CSI, USB, and configured IP sources).

    On macOS and other non-Linux platforms, only IP sources are returned.
    """
    system = platform.system()
    cameras: list[CameraInfo] = []

    if system == "Linux":
        cameras.extend(_discover_csi_cameras())
        cameras.extend(_discover_usb_cameras())
    else:
        log.debug("camera_discovery_skip", platform=system, msg="CSI/USB discovery requires Linux")

    if ip_sources:
        cameras.extend(_cameras_from_config(ip_sources))

    log.info("camera_discovery_complete", total=len(cameras), platform=system)
    return cameras
