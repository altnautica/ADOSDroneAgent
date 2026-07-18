"""Camera discovery — CSI (rpicam), USB (v4l2), and IP cameras."""

from __future__ import annotations

import errno
import json
import os
import platform
import re
import subprocess
import time
from dataclasses import dataclass, field
from enum import StrEnum

from ados.core.logging import get_logger

log = get_logger("hal.camera")

# Discovery sidecar the enumeration writes and the native camera-roster route
# (ados-control `GET /api/video/cameras`) reads, so the serve path stays
# subprocess-free. The roster reconciles this against the declared
# `video.cameras[]` and the live `video-streams.json`. Overridable via
# ADOS_RUN_DIR for tests / a non-default runtime dir.
CAMERAS_DISCOVERED_JSON_NAME = "cameras-discovered.json"

# Schema version of the discovery sidecar. Readers compare best-effort and read
# anyway (additive fields never break an older reader).
CAMERAS_DISCOVERED_VERSION = 1


def _run_dir() -> str:
    return os.environ.get("ADOS_RUN_DIR", "/run/ados")


def cameras_discovered_path() -> str:
    """Canonical path of the discovery sidecar under the runtime dir."""
    return os.path.join(_run_dir(), CAMERAS_DISCOVERED_JSON_NAME)


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
    # Physical fingerprint the camera roster reconciles a declared leg against
    # when the device node has been renamed by a hot-plug/reboot: a USB camera
    # carries ``{"usb": "vid:pid[:serial]"}``, a CSI camera carries
    # ``{"csi_sensor": <name>, "csi_port": <index>}``. Empty when the fingerprint
    # could not be read (best-effort; the roster then keys on the device path).
    match: dict = field(default_factory=dict)

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "type": self.type.value,
            "device_path": self.device_path,
            "width": self.width,
            "height": self.height,
            "capabilities": self.capabilities,
            "hardware_role": self.hardware_role.value,
            "match": self.match,
        }


def _read_sysfs(path: str) -> str | None:
    """Read a single-line sysfs attribute, stripped, or None on any failure."""
    try:
        with open(path, encoding="utf-8") as fh:
            value = fh.read().strip()
    except OSError:
        return None
    return value or None


def _usb_match(device_path: str) -> dict:
    """Best-effort USB fingerprint for a ``/dev/videoN`` node.

    Resolves the video node's sysfs device link, then walks up the device tree
    to the USB device directory that carries ``idVendor`` / ``idProduct`` (the
    parent of the UVC interface), reading an optional ``serial``. Returns
    ``{"usb": "vid:pid"}`` or ``{"usb": "vid:pid:serial"}`` (lowercase hex vid/
    pid), or ``{}`` when the sysfs path is absent (non-USB, non-Linux, or a
    permission failure) — the roster then keys on the device path alone.
    """
    node = os.path.basename(device_path.rstrip("/"))
    base = f"/sys/class/video4linux/{node}/device"
    try:
        dev = os.path.realpath(base)
    except OSError:
        return {}
    if not os.path.isdir(dev):
        return {}
    for _ in range(8):
        vid = _read_sysfs(os.path.join(dev, "idVendor"))
        pid = _read_sysfs(os.path.join(dev, "idProduct"))
        if vid and pid:
            fp = f"{vid.lower()}:{pid.lower()}"
            serial = _read_sysfs(os.path.join(dev, "serial"))
            if serial:
                fp = f"{fp}:{serial}"
            return {"usb": fp}
        parent = os.path.dirname(dev)
        if parent == dev:
            break
        dev = parent
    return {}


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
                match={"csi_sensor": sensor, "csi_port": int(idx)},
            ))

        if cameras:
            log.info("csi_cameras_found", count=len(cameras))
    except FileNotFoundError:
        log.debug("rpicam_not_found", msg="rpicam-hello not in PATH")
    except subprocess.TimeoutExpired:
        log.warning("rpicam_timeout")

    return cameras


def _video_node_openable(path: str) -> bool:
    """Confirm a /dev/videoN node still backs a live, openable device.

    A node left behind by a recently unplugged USB camera keeps showing
    up in `v4l2-ctl --list-devices` output for a short window but can no
    longer be opened. Adding it as a camera makes the pipeline launch an
    encoder against a dead device, which floods stderr and churns
    restarts until the process runs out of file descriptors. Opening the
    node non-blocking (then closing it immediately) is the cheapest
    reliable liveness check — no subprocess, no streaming side effects.

    EBUSY means a real device is present but already claimed (e.g. by the
    running encoder), so treat that as present, not stale.
    """
    try:
        fd = os.open(path, os.O_RDWR | os.O_NONBLOCK)
    except OSError as exc:
        return exc.errno == errno.EBUSY
    os.close(fd)
    return True


def _discover_usb_cameras() -> list[CameraInfo]:
    """Detect USB cameras via v4l2-ctl --list-devices.

    v4l2-ctl exits non-zero when any /dev/videoN node fails to open
    (a stale node from a recently unplugged device, for example), but
    still prints valid cameras to stdout. Parse stdout regardless of
    exit code so the good data is not thrown away, then probe-open each
    candidate node so a stale ghost from an unplugged camera is dropped
    instead of handed to the encoder.

    UVC cameras create two /dev/videoN nodes per physical camera (a main
    capture stream and a metadata stream). Adding all of them as separate
    CameraInfo entries causes auto_assign() to pick one as primary and
    one as secondary while both point at the same physical device. Only
    the first /dev/videoN in each device-name block becomes a CameraInfo;
    later nodes are metadata or alternates.
    """
    cameras: list[CameraInfo] = []
    try:
        result = subprocess.run(
            ["v4l2-ctl", "--list-devices"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        # parse stdout regardless of returncode (see docstring)

        # Parse blocks: device name on one line, /dev/videoN on next indented lines
        current_name = ""
        block_consumed = False  # one CameraInfo per device block
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
                # v4l2-ctl --list-devices returns the kernel's video subsystem,
                # which on a Raspberry Pi includes the bcm2835 codec / ISP /
                # hevc decoder / unicam capture backplane alongside any real
                # USB UVC camera. Those internal devices are not capturable
                # cameras for our purposes — skip them entirely so they do
                # not pollute the camera list surfaced through /api/video,
                # the wizard, or the GCS Hardware tab. The CSI capture path
                # is reported separately by `_discover_csi_cameras` via
                # rpicam-hello, which is the canonical source for CSI input.
                name_lower = (current_name or "").lower()
                if (
                    "codec" in name_lower
                    or "isp" in name_lower
                    or "hevc" in name_lower
                    or "rpivid" in name_lower
                    or "unicam" in name_lower
                ):
                    block_consumed = True
                    continue
                if not _video_node_openable(stripped):
                    # Stale node from a recently unplugged camera. Skip it
                    # without consuming the block so a sibling node in the
                    # same device can still be tried.
                    log.debug(
                        "usb_camera_node_unopenable",
                        device=stripped,
                        name=current_name,
                        msg="node listed but failed to open; treating as absent",
                    )
                    continue
                cameras.append(CameraInfo(
                    name=current_name or "USB Camera",
                    type=CameraType.USB,
                    device_path=stripped,
                    capabilities=["mjpeg", "yuyv"],
                    hardware_role=HardwareRole.CAMERA,
                    match=_usb_match(stripped),
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


def write_discovery_sidecar(
    cameras: list[CameraInfo],
    path: str | None = None,
) -> bool:
    """Atomically write the discovery sidecar the camera-roster route reads.

    The payload is ``{version, updated_at_unix, cameras: [CameraInfo.to_dict()]}``.
    Writing it here keeps the roster serve path (the native Rust route) free of a
    per-request subprocess — the enumeration runs when the pipeline (re)starts and
    the route merges the sidecar against the declared legs and the live streams.

    Best-effort: returns True on success, False on any I/O error (a read-only or
    absent runtime dir on a dev host), never raising so the enumeration seam it
    rides on is unaffected.
    """
    target = path or cameras_discovered_path()
    payload = {
        "version": CAMERAS_DISCOVERED_VERSION,
        "updated_at_unix": time.time(),
        "cameras": [c.to_dict() for c in cameras],
    }
    try:
        parent = os.path.dirname(target)
        if parent:
            os.makedirs(parent, exist_ok=True)
        tmp = f"{target}.tmp"
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump(payload, fh)
            fh.flush()
            os.fsync(fh.fileno())
        os.chmod(tmp, 0o644)
        os.replace(tmp, target)
        return True
    except OSError as exc:
        log.debug("camera_discovery_sidecar_write_failed", path=target, error=str(exc))
        return False


if __name__ == "__main__":
    # `python -m ados.hal.camera --json`: one-shot discovery for the video
    # orchestrator seam. The body lives in camera_cli so this module stays a
    # pure library import for everything else.
    from ados.hal.camera_cli import main

    raise SystemExit(main())
