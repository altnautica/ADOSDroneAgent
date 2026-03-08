"""JPEG snapshot capture with optional EXIF GPS metadata."""

from __future__ import annotations

import asyncio
import shutil
from datetime import datetime, timezone
from pathlib import Path

from ados.core.logging import get_logger
from ados.hal.camera import CameraInfo, CameraType

log = get_logger("video.snapshot")


def _build_capture_command(camera: CameraInfo, output_path: str) -> list[str]:
    """Build the subprocess command for a single-frame capture."""
    if camera.type == CameraType.CSI and shutil.which("rpicam-still"):
        return [
            "rpicam-still",
            "--nopreview",
            "--timeout", "1000",
            "-o", output_path,
        ]

    # Fallback to ffmpeg for USB / IP cameras or when rpicam-still is absent
    source = camera.device_path
    input_format_args: list[str] = []
    if camera.type == CameraType.USB:
        input_format_args = ["-f", "v4l2"]
    elif camera.type == CameraType.IP:
        input_format_args = ["-rtsp_transport", "tcp"]

    return [
        "ffmpeg",
        "-y",
        *input_format_args,
        "-i", source,
        "-frames:v", "1",
        "-q:v", "2",
        output_path,
    ]


def _write_exif(path: str, gps_lat: float, gps_lon: float) -> bool:
    """Write GPS EXIF data to a JPEG file if piexif is available.

    Returns True if EXIF was written successfully, False otherwise.
    """
    try:
        import piexif
    except ImportError:
        log.debug("piexif_not_available", msg="EXIF metadata skipped")
        return False

    def _to_dms(coord: float) -> tuple[tuple[int, int], tuple[int, int], tuple[int, int]]:
        """Convert decimal degrees to degrees/minutes/seconds as rational tuples.

        Uses a denominator of 1,000,000 for seconds to preserve sub-arcsecond
        precision (~0.03mm at the equator), avoiding float-to-int truncation
        that would otherwise lose significant digits.
        """
        abs_coord = abs(coord)
        deg = int(abs_coord)
        min_float = (abs_coord - deg) * 60
        minutes = int(min_float)
        sec_float = (min_float - minutes) * 60
        # Use 1,000,000 denominator for sub-arcsecond precision
        seconds = round(sec_float * 1_000_000)
        return ((deg, 1), (minutes, 1), (seconds, 1_000_000))

    lat_ref = b"N" if gps_lat >= 0 else b"S"
    lon_ref = b"E" if gps_lon >= 0 else b"W"

    gps_ifd = {
        piexif.GPSIFD.GPSLatitudeRef: lat_ref,
        piexif.GPSIFD.GPSLatitude: _to_dms(gps_lat),
        piexif.GPSIFD.GPSLongitudeRef: lon_ref,
        piexif.GPSIFD.GPSLongitude: _to_dms(gps_lon),
    }

    now = datetime.now(timezone.utc)
    zeroth_ifd = {
        piexif.ImageIFD.DateTime: now.strftime("%Y:%m:%d %H:%M:%S").encode(),
        piexif.ImageIFD.Software: b"ADOS Drone Agent",
    }

    exif_dict = {"0th": zeroth_ifd, "GPS": gps_ifd}

    try:
        exif_bytes = piexif.dump(exif_dict)
        piexif.insert(exif_bytes, path)
        log.debug("exif_written", path=path, lat=gps_lat, lon=gps_lon)
        return True
    except Exception as exc:
        log.warning("exif_write_failed", path=path, error=str(exc))
        return False


async def capture_snapshot(
    camera: CameraInfo,
    output_dir: str,
    gps_lat: float = 0.0,
    gps_lon: float = 0.0,
) -> str:
    """Capture a single JPEG frame from a camera.

    Args:
        camera: The camera to capture from.
        output_dir: Directory to save the JPEG file.
        gps_lat: GPS latitude for EXIF metadata (decimal degrees).
        gps_lon: GPS longitude for EXIF metadata (decimal degrees).

    Returns:
        The path to the captured JPEG file, or empty string on failure.
    """
    out_path = Path(output_dir)
    out_path.mkdir(parents=True, exist_ok=True)

    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    filename = f"snapshot_{ts}.jpg"
    filepath = str(out_path / filename)

    cmd = _build_capture_command(camera, filepath)
    log.info("snapshot_capture_start", camera=camera.name, output=filepath)

    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await asyncio.wait_for(proc.communicate(), timeout=30.0)

        if proc.returncode != 0:
            err_msg = stderr.decode(errors="replace")[:200] if stderr else "unknown error"
            log.error("snapshot_capture_failed", error=err_msg)
            return ""

    except FileNotFoundError:
        log.error("snapshot_tool_not_found", cmd=cmd[0])
        return ""
    except TimeoutError:
        log.error("snapshot_capture_timeout", camera=camera.name)
        return ""

    # Write EXIF if GPS coords are provided
    if gps_lat != 0.0 or gps_lon != 0.0:
        _write_exif(filepath, gps_lat, gps_lon)

    log.info("snapshot_captured", path=filepath)
    return filepath
