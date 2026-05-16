"""Video pipeline configuration."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

from ados.core.paths import RECORDINGS_DIR

from .wfb import WfbConfig


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
    # for wfb_tx. Default off until bench-validated; flip per-rig in
    # /etc/ados/config.yaml under ``video.use_gst_air_pipeline`` and
    # restart the agent. When false, the legacy bash path is unchanged.
    use_gst_air_pipeline: bool = False
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
