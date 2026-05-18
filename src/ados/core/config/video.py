"""Video pipeline configuration."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

from ados.core.paths import RECORDINGS_DIR

from .wfb import WfbConfig


def _default_use_gst_air_pipeline() -> bool:
    """Resolve the per-board default for ``use_gst_air_pipeline``.

    Resolution order:

    1. Runtime self-heal override at ``/run/ados/video-encoder-override.yaml``.
       Written by the AirPipeline health watcher when it detects an
       unhealthy bus_errors rate. Wins over the board default so a
       crash-looping hardware encoder auto-disables itself without
       operator intervention.
    2. Per-board fingerprint. Returns True on Rockchip boards (RK3566
       / RK3582 / RK3588 / …) where the in-process GStreamer pipeline
       can plug into the vendor ``mpph264enc`` element and offload
       H.264 encoding to the VPU, cutting CPU from ~48 % (libx264) to
       <10 % at 720p30 4 Mbps.
    3. False everywhere else so other boards stay on the
       bench-validated legacy bash pipeline path.

    An explicit ``video.use_gst_air_pipeline: true|false`` in
    ``/etc/ados/config.yaml`` bypasses this factory entirely and
    always wins. Operator intent is sacred — the runtime override
    can only adjust the per-board default, never an explicit value.

    All probes are wrapped in broad try/except so config loading never
    blocks on /proc parsing failures, a missing override file, or
    an unavailable HAL.
    """
    # Layer 1: runtime self-heal override (set by the air pipeline
    # bus_errors watchdog when it detects a crash-looping HW encoder).
    try:
        from ados.services.video.air_pipeline.auto_fallback import (
            is_auto_fallback_active,
        )

        if is_auto_fallback_active():
            return False
    except Exception:
        pass

    # Layer 2: per-board fingerprint.
    try:
        from ados.hal.detect import detect_board

        board = detect_board()
        soc = (getattr(board, "soc", None) or "").lower()
        # Rockchip SoCs identify as rk3566, rk3568, rk3582, rk3588, etc.
        return soc.startswith("rk")
    except Exception:
        # Any failure (HAL not importable in a unit-test fixture, /proc
        # unreadable in a container) falls back to legacy bash, never
        # crashes config load.
        return False


class CameraConfig(BaseModel):
    source: str = "csi"
    codec: str = "h264"
    width: int = 1280
    height: int = 720
    fps: int = 30
    bitrate_kbps: int = 4000
    # Operator preference for the wire codec. "auto" picks H.264 by
    # default for browser-compat with WHEP; on boards whose HAL
    # advertises h265_enc the encoder will offer libx265 / mpph265enc
    # / hevc_v4l2m2m, but flipping to H.265 requires the receiver
    # side to also speak it (LCD decoder + browser MediaCapabilities).
    # Switching from h264 to h265 trades ~5-15 ms of wire latency
    # and ~40-50% bitrate against the loss of Firefox / older Chrome
    # Linux support. Default stays h264 until the full dual-codec
    # WHEP negotiation lands.
    codec_preference: Literal["h264", "h265", "auto"] = "auto"


class RecordingConfig(BaseModel):
    enabled: bool = False
    path: str = str(RECORDINGS_DIR)
    max_duration_minutes: int = 30


class VideoConfig(BaseModel):
    mode: str = "wfb"
    wfb: WfbConfig = WfbConfig()
    camera: CameraConfig = CameraConfig()
    recording: RecordingConfig = RecordingConfig()
    cloud_relay_url: str = ""  # e.g. rtsp://video.altnautica.com:8554
    # Decoded-frame cap fed into the LCD GStreamer pipeline (videorate
    # decimation target). Higher = smoother LCD video at the cost of
    # CPU on the SPI render loop. Default 15 matches Pi 4B + Waveshare
    # 3.5" SPI throughput; faster SBCs / hardware decoders / lighter
    # render paths can raise it. Bench-validate before flipping the
    # default in repo.
    lcd_fps_cap: int = Field(
        default=15,
        ge=1,
        le=60,
    )
    # Opt into the in-process GStreamer air-side pipeline that replaces
    # the legacy bash composition of camera-capture + ffmpeg-encoder +
    # mediamtx-air + ffmpeg-tee + python-sei-injector + ffmpeg-RTP with a
    # single PyGObject-driven pipeline writing RTP directly to UDP 5600
    # for wfb_tx. Default is **per-board**: True on Rockchip SoCs where
    # ``mpph264enc`` offloads H.264 to the VPU, False everywhere else so
    # we stay on the bench-validated bash pipeline. AirPipeline's own
    # ``choose_encoder()`` then transparently falls back to ``x264enc``
    # (software) if ``mpph264enc`` isn't available at runtime. Explicit
    # ``video.use_gst_air_pipeline: true|false`` in
    # ``/etc/ados/config.yaml`` always overrides the per-board default.
    # The auto-fallback watcher in ``air_pipeline/auto_fallback.py``
    # can also flip this to False at runtime if it detects a misbehaving
    # hardware encoder (the override lives on /run tmpfs so a reboot
    # gives the rig back its per-board default).
    use_gst_air_pipeline: bool = Field(
        default_factory=_default_use_gst_air_pipeline,
    )
    # When the GStreamer pipeline can choose between a hardware encoder
    # (v4l2h264enc on Pi, mpph264enc on Rockchip, omxh264enc / nvv4l2h264enc
    # on Jetson) and the software libx264 fallback, prefer hardware. Set
    # false to force software for A/B benchmarking or when the hardware
    # path is known fragile on a given rig.
    prefer_hw_encoder: bool = True
    # UDP port the GStreamer pipeline emits a second RTP copy to when
    # cloud relay is enabled. mediamtx-air's udpRead ingest binds the
    # same port and republishes as RTSP/WHEP for the browser preview.
    # Only used when ``cloud_relay_url`` is also set; otherwise the
    # branch is muted at the pipeline's identity-element gate and no
    # packets leave the loopback interface.
    cloud_rtp_port: int = 8000
