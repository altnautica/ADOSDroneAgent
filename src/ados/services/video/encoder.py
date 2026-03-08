"""Encoder abstraction — builds command lines for rpicam-vid, ffmpeg, or GStreamer."""

from __future__ import annotations

import shutil
from dataclasses import dataclass
from enum import StrEnum

from ados.core.logging import get_logger

log = get_logger("video.encoder")


class EncoderType(StrEnum):
    RPICAM_VID = "rpicam-vid"
    FFMPEG = "ffmpeg"
    GSTREAMER = "gstreamer"


@dataclass
class EncoderConfig:
    """Parameters for a video encoder invocation."""

    type: EncoderType
    codec: str = "h264"
    width: int = 1280
    height: int = 720
    fps: int = 30
    bitrate_kbps: int = 4000


def detect_available_encoder() -> EncoderType | None:
    """Detect which encoder binary is available on this system.

    Checks in order of preference: rpicam-vid, ffmpeg, gst-launch-1.0.
    """
    if shutil.which("rpicam-vid"):
        log.info("encoder_detected", encoder="rpicam-vid")
        return EncoderType.RPICAM_VID
    if shutil.which("ffmpeg"):
        log.info("encoder_detected", encoder="ffmpeg")
        return EncoderType.FFMPEG
    if shutil.which("gst-launch-1.0"):
        log.info("encoder_detected", encoder="gstreamer")
        return EncoderType.GSTREAMER
    log.warning("no_encoder_found")
    return None


def build_encoder_command(
    config: EncoderConfig,
    source: str,
    output: str,
) -> list[str]:
    """Build a subprocess command list for the given encoder configuration.

    Args:
        config: Encoder settings (type, codec, resolution, etc.).
        source: Input source (device path, URL, or ``-`` for stdin).
        output: Output destination (file path, pipe URI, or ``-`` for stdout).

    Returns:
        Command list suitable for ``subprocess.Popen`` or ``asyncio.create_subprocess_exec``.
    """
    if config.type == EncoderType.RPICAM_VID:
        return _build_rpicam_command(config, source, output)
    if config.type == EncoderType.FFMPEG:
        return _build_ffmpeg_command(config, source, output)
    if config.type == EncoderType.GSTREAMER:
        return _build_gstreamer_command(config, source, output)
    return []


def _build_rpicam_command(
    config: EncoderConfig,
    source: str,
    output: str,
) -> list[str]:
    """rpicam-vid command for CSI camera encoding."""
    cmd = [
        "rpicam-vid",
        "--width", str(config.width),
        "--height", str(config.height),
        "--framerate", str(config.fps),
        "--bitrate", str(config.bitrate_kbps * 1000),
        "--codec", config.codec,
        "--timeout", "0",
        "--nopreview",
    ]
    if source and source != "-":
        cmd.extend(["--camera", source])
    cmd.extend(["-o", output])
    return cmd


def _build_ffmpeg_command(
    config: EncoderConfig,
    source: str,
    output: str,
) -> list[str]:
    """ffmpeg command for generic encoding."""
    codec_map = {
        "h264": "libx264",
        "h265": "libx265",
        "hevc": "libx265",
        "mjpeg": "mjpeg",
    }
    ffmpeg_codec = codec_map.get(config.codec, "libx264")

    cmd = [
        "ffmpeg",
        "-y",
        "-f", "v4l2",
        "-video_size", f"{config.width}x{config.height}",
        "-framerate", str(config.fps),
        "-i", source,
        "-c:v", ffmpeg_codec,
        "-b:v", f"{config.bitrate_kbps}k",
    ]
    if ffmpeg_codec == "libx264":
        cmd.extend(["-preset", "ultrafast", "-tune", "zerolatency"])
    cmd.append(output)
    return cmd


def _build_gstreamer_command(
    config: EncoderConfig,
    source: str,
    output: str,
) -> list[str]:
    """GStreamer pipeline command."""
    encoder_element = "x264enc" if config.codec in ("h264", "H264") else "x265enc"
    pipeline = (
        f"v4l2src device={source} ! "
        f"video/x-raw,width={config.width},height={config.height},"
        f"framerate={config.fps}/1 ! "
        f"videoconvert ! {encoder_element} bitrate={config.bitrate_kbps} "
        f"tune=zerolatency ! "
        f"filesink location={output}"
    )
    return ["gst-launch-1.0", "-e", *pipeline.split()]
