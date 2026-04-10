"""Encoder abstraction — builds command lines for rpicam-vid, ffmpeg, or GStreamer.

Handles CSI, USB, and IP cameras with hardware/software encoding fallbacks.
Camera capabilities (from HAL discovery) drive input format selection for
optimal framerate and CPU usage.
"""

from __future__ import annotations

import re
import shlex
import shutil
import subprocess
from dataclasses import dataclass
from enum import StrEnum

from ados.core.logging import get_logger
from ados.hal.camera import CameraInfo, CameraType

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


def detect_encoder_for_camera(camera: CameraInfo) -> EncoderType | None:
    """Pick the right encoder based on camera type.

    CSI cameras use rpicam-vid (native Pi hardware encoder), falling back to ffmpeg.
    USB and IP cameras use ffmpeg, falling back to GStreamer.
    """
    if camera.type == CameraType.CSI:
        if shutil.which("rpicam-vid"):
            log.info("encoder_selected", encoder="rpicam-vid", reason="csi_camera")
            return EncoderType.RPICAM_VID
        if shutil.which("ffmpeg"):
            log.info("encoder_selected", encoder="ffmpeg", reason="csi_fallback")
            return EncoderType.FFMPEG
    elif camera.type in (CameraType.USB, CameraType.IP):
        if shutil.which("ffmpeg"):
            log.info("encoder_selected", encoder="ffmpeg", reason=f"{camera.type.value}_camera")
            return EncoderType.FFMPEG
        if shutil.which("gst-launch-1.0"):
            log.info("encoder_selected", encoder="gstreamer", reason=f"{camera.type.value}_fallback")
            return EncoderType.GSTREAMER
    log.warning("no_encoder_for_camera", camera_type=camera.type.value)
    return None


def _detect_hw_h264_encoder() -> str | None:
    """Check if ffmpeg has a hardware H.264 encoder available.

    Returns the encoder name (e.g., 'h264_v4l2m2m' for Pi, 'h264_nvenc' for Jetson)
    or None if only software encoding is available.
    """
    # DEC-106 Bug #3: ffmpeg's h264_v4l2m2m plugin is listed in -encoders
    # output on Rockchip SoCs because the plugin library is present, but it
    # cannot find the rkmpp encoder device and hangs in an uninterruptible
    # subprocess.wait() when probed. Force libx264 software encoder fallback
    # on Rockchip by short-circuiting BEFORE the ffmpeg probe runs.
    #
    # TODO(follow-up PR): detect gstreamer mpph264enc via
    #   `gst-inspect-1.0 mpph264enc` and emit a GStreamer-based encoder
    #   command instead of ffmpeg on Rockchip. libx264 CPU cost is fine
    #   on Rock 5C Lite (~48% at 1280x720@30) but wastes the VPU.
    try:
        with open("/proc/device-tree/compatible", "rb") as _f:
            if b"rockchip" in _f.read():
                return None
    except Exception:
        pass

    try:
        result = subprocess.run(
            ["ffmpeg", "-hide_banner", "-encoders"],
            capture_output=True, text=True, timeout=5,
        )
        output = result.stdout
        # Check hardware encoders in order of preference
        hw_encoders = ["h264_v4l2m2m", "h264_nvenc", "h264_vaapi", "h264_omx"]
        for enc in hw_encoders:
            if enc in output:
                return enc
    except Exception:
        pass
    return None


# Allowlist: alphanumeric, slashes, dots, hyphens, underscores, colons
_SAFE_SOURCE_PATTERN = re.compile(r"^[a-zA-Z0-9/_.\-:]+$")


def _validate_source(source: str) -> str:
    """Validate and sanitize a camera source path.

    Raises:
        ValueError: If the source contains disallowed characters.
    """
    if source == "-":
        return source
    if not _SAFE_SOURCE_PATTERN.match(source):
        raise ValueError(
            f"Invalid source path: {source!r}. "
            "Only alphanumeric, slashes, dots, hyphens,"
            " underscores, and colons are allowed."
        )
    return source


def build_encoder_command(
    config: EncoderConfig,
    source: str,
    output: str,
    camera: CameraInfo | None = None,
) -> list[str]:
    """Build a subprocess command list for the given encoder configuration.

    Args:
        config: Encoder settings (type, codec, resolution, etc.).
        source: Input source (device path, URL, or ``-`` for stdin).
        output: Output destination (file path, pipe URI, or ``-`` for stdout).
        camera: Optional camera info for capability-aware input format selection.

    Returns:
        Command list suitable for ``subprocess.Popen`` or ``asyncio.create_subprocess_exec``.

    Raises:
        ValueError: If the source path contains disallowed characters.
    """
    source = _validate_source(source)
    output = _validate_source(output)
    if config.type == EncoderType.RPICAM_VID:
        return _build_rpicam_command(config, source, output)
    if config.type == EncoderType.FFMPEG:
        return _build_ffmpeg_command(config, source, output, camera)
    if config.type == EncoderType.GSTREAMER:
        return _build_gstreamer_command(config, source, output)
    return []


def _build_rpicam_command(
    config: EncoderConfig,
    source: str,
    output: str,
) -> list[str]:
    """rpicam-vid command for CSI camera encoding.

    Uses the Pi's hardware VideoCore encoder for zero-CPU H.264/H.265.
    """
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
        # rpicam-vid expects camera index (0, 1, ...) not device path
        if source.startswith("/dev/video"):
            cam_idx = source.replace("/dev/video", "")
        else:
            cam_idx = source
        cmd.extend(["--camera", cam_idx])
    cmd.extend(["-o", output])
    return cmd


def _select_input_format(camera: CameraInfo | None) -> str | None:
    """Choose the best V4L2 input format based on camera capabilities.

    Priority: mjpeg (compressed, high fps, low USB bandwidth) > yuyv (raw, lower fps).
    Returns None if capabilities are unknown (let ffmpeg auto-detect).
    """
    if camera is None:
        return None
    caps = [c.lower() for c in camera.capabilities]
    if "mjpeg" in caps or "mjpg" in caps:
        return "mjpeg"
    if "yuyv" in caps or "rawvideo" in caps:
        return "yuyv"
    return None


def _build_ffmpeg_command(
    config: EncoderConfig,
    source: str,
    output: str,
    camera: CameraInfo | None = None,
) -> list[str]:
    """ffmpeg command for USB/IP camera encoding.

    Input format selection:
      - USB cameras: prefer MJPG for 30fps (vs 5-10fps raw YUYV)
      - IP cameras (RTSP): no V4L2 wrapper needed

    Encoder selection:
      - Hardware H.264 (v4l2m2m, nvenc, vaapi) if available
      - Software libx264 ultrafast as fallback
    """
    # Select output codec — try hardware first for H.264
    hw_encoder = None
    if config.codec in ("h264", "H264"):
        hw_encoder = _detect_hw_h264_encoder()

    if hw_encoder:
        ffmpeg_codec = hw_encoder
        log.info("hw_encoder_selected", encoder=hw_encoder)
    else:
        codec_map = {
            "h264": "libx264",
            "h265": "libx265",
            "hevc": "libx265",
            "mjpeg": "mjpeg",
        }
        ffmpeg_codec = codec_map.get(config.codec, "libx264")

    cmd = ["ffmpeg", "-y"]

    if source.startswith("rtsp://") or source.startswith("http://"):
        # Network/IP camera source
        cmd.extend(["-i", source])
    else:
        # V4L2 device — select best input format from camera capabilities.
        #
        # Low-latency flags (before -i so they apply to the input demuxer):
        #   -fflags nobuffer    : do not buffer input frames; hand them to the
        #                         encoder as soon as they arrive from the camera
        #   -flags low_delay    : hint the codec layer to prefer low delay
        #   -probesize 32       : reduce stream probing to 32 bytes (V4L2
        #                         streams have a fixed format, no need to probe)
        #   -analyzeduration 0  : skip analysis phase (same reason as above)
        #   -thread_queue_size 4: shrink the input demux queue from the default
        #                         8 to 4 — fewer buffered frames = less latency
        input_fmt = _select_input_format(camera)
        cmd.extend([
            "-fflags", "nobuffer",
            "-flags", "low_delay",
            "-probesize", "32",
            "-analyzeduration", "0",
            "-thread_queue_size", "4",
            "-f", "v4l2",
        ])
        if input_fmt:
            cmd.extend(["-input_format", input_fmt])
        cmd.extend([
            "-video_size", f"{config.width}x{config.height}",
            "-framerate", str(config.fps),
            "-i", source,
        ])

    cmd.extend([
        "-c:v", ffmpeg_codec,
        "-b:v", f"{config.bitrate_kbps}k",
    ])

    # Encoder-specific tuning
    if ffmpeg_codec == "libx264":
        # DEC-108 Phase A: force browser-compatible H.264 output.
        #
        # Without -pix_fmt yuv420p, libx264 inherits the chroma from the
        # input. USB UVC cameras commonly send YUYV422, so libx264 produces
        # `High 4:2:2 profile` H.264 — which browser WebRTC stacks REJECT
        # outright. The GCS's MSE player (mse-player.ts) hardcodes the
        # decoder string to `avc1.640029` = H.264 High profile, level 4.1,
        # 4:2:0 chroma. This pins the encoder to that exact profile.
        #
        # -g matches keyframe interval to fps (1s GOP) so WebRTC and MSE
        # players see a fresh keyframe within ~1s of subscribing — without
        # this, late joiners stare at black until the next IDR (~10s).
        #
        # Validated on Rock 5C Lite bench 2026-04-09: HZ USB Camera
        # produced High 4:2:2 with the unfixed encoder, browser couldn't
        # decode. With this fix the stream is High 4:2:0 level 4.1.
        cmd.extend([
            "-pix_fmt", "yuv420p",
            "-profile:v", "high",
            "-level:v", "4.1",
            "-preset", "ultrafast",
            "-tune", "zerolatency",
            "-g", str(config.fps),
            # DEC-108 Phase E: convert MP4-style NAL framing to Annex-B for
            # downstream RTSP/WebRTC compatibility. libx264 produces AVCC
            # length-prefixed NALs by default; mediamtx (and browser WebRTC
            # decoders that mediamtx feeds) expect Annex-B start codes.
            # Without this we saw `inboundFramesInError` climbing on
            # mediamtx — those were NAL parse failures at the RTSP boundary.
            "-bsf:v", "h264_mp4toannexb",
        ])
    elif ffmpeg_codec == "h264_v4l2m2m":
        # Pi V4L2 M2M needs yuv420p input — force pixel format conversion
        cmd.extend(["-pix_fmt", "yuv420p", "-g", str(config.fps)])

    # Specify output format for network destinations
    if output.startswith("rtsp://"):
        # DEC-108 Phase E: force RTSP output transport to TCP. Default is
        # UDP which fragments large H.264 NALs across multiple datagrams;
        # with `-tune zerolatency` libx264 keyframes spike to 8-12 Mbps and
        # the fragmentation causes mediamtx reassembly errors. TCP RTSP
        # eliminates the fragmentation issue (cost: marginally higher
        # latency, irrelevant on localhost).
        #
        # -max_delay 0: flush encoded frames to the muxer immediately
        # instead of waiting for the muxer's default interleave buffer.
        cmd.extend(["-max_delay", "0", "-rtsp_transport", "tcp", "-f", "rtsp"])
    elif output.startswith("udp://") or output.startswith("tcp://"):
        cmd.extend(["-f", "mpegts"])

    cmd.append(output)
    return cmd


def _build_gstreamer_command(
    config: EncoderConfig,
    source: str,
    output: str,
) -> list[str]:
    """GStreamer pipeline command (last-resort fallback)."""
    encoder_element = "x264enc" if config.codec in ("h264", "H264") else "x265enc"
    safe_source = shlex.quote(source)

    # Build pipeline for RTSP output via rtspclientsink, or file output
    if output.startswith("rtsp://"):
        safe_output = shlex.quote(output)
        pipeline = (
            f"v4l2src device={safe_source} ! "
            f"image/jpeg,width={config.width},height={config.height},"
            f"framerate={config.fps}/1 ! jpegdec ! videoconvert ! "
            f"{encoder_element} bitrate={config.bitrate_kbps} "
            f"tune=zerolatency ! h264parse ! "
            f"rtspclientsink location={safe_output}"
        )
    else:
        safe_output = shlex.quote(output)
        pipeline = (
            f"v4l2src device={safe_source} ! "
            f"image/jpeg,width={config.width},height={config.height},"
            f"framerate={config.fps}/1 ! jpegdec ! videoconvert ! "
            f"{encoder_element} bitrate={config.bitrate_kbps} "
            f"tune=zerolatency ! filesink location={safe_output}"
        )
    return ["gst-launch-1.0", "-e", *pipeline.split()]
