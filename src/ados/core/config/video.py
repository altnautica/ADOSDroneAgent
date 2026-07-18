"""Video pipeline configuration."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, ConfigDict, Field

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
    # Whether a primary camera is expected on this rig, for the supervisor's
    # camera USB-recovery reconciler. "auto" (default) treats a camera as
    # expected once one has enumerated successfully at least once (a persisted
    # last-known-good record exists), so a camera-less drone never triggers a
    # spurious recovery while a cold-boot enumeration failure still does.
    expected: Literal["auto", "true", "false"] = "auto"


class CameraMatch(BaseModel):
    """A physical fingerprint that re-pins a leg's logical ``id`` onto its
    current ``source`` device after a hot-plug / reboot renamed the device node.
    USB cameras carry a ``vid:pid[:serial]`` string; CSI cameras carry the sensor
    name plus the camera port index. Every field is optional — an absent match
    means the leg is pinned only by its ``source`` locator (a network URL never
    moves). Mirrors the Rust ``CameraMatch``.
    """

    usb: str | None = None
    csi_sensor: str | None = None
    csi_port: int | None = None


class CameraLeg(BaseModel):
    """One entry of the optional ``video.cameras`` list — a single video leg the
    node exposes as its own mediamtx path (and ``:8889/<id>/whep``). Present only
    when more than one stream is declared (a smart pod, a dual-camera rig); an
    absent list falls back to the single ``video.camera`` block. The primary leg
    is always served at the fixed ``main`` path; secondary legs keep their ids.

    ``id`` is the leg's immutable logical identity; ``source`` is its current
    locator; ``match`` re-pins ``source`` → ``id`` across a device rename.
    ``role`` is the transport plane (primary → the fixed ``main`` path / WFB /
    cloud); ``purpose`` is the consumer plane a plugin binds to. The management
    fields (``name`` / ``orientation`` / ``purpose`` / ``enabled`` / ``owner`` /
    ``fov_deg`` / ``mount_pitch_deg`` / ``calibration`` / ``match``) are metadata
    surfaced through the camera roster; the encode + radio pipeline reads none of
    them, so an existing single-``role`` config resolves byte-identically.
    """

    id: str = "main"
    source: str = "csi"
    # Logical role: "primary" designates the WFB/cloud stream; any other value
    # (or absent) is a LAN-WHEP-only secondary. Absent on every leg → the first
    # leg is the primary.
    role: str | None = None
    codec: str = "h264"
    width: int = 1280
    height: int = 720
    fps: int = 30
    bitrate_kbps: int = 4000
    # --- Camera-roster management metadata (consumed by the roster + plugins,
    # never by the encode/radio pipeline; additive and default-safe). ---
    # Operator-facing display name for the roster.
    name: str | None = None
    # Coarse physical mount orientation: forward | down | back | left | right |
    # up | gimbal | custom. Enough for plugin binding, not full extrinsics.
    orientation: str | None = None
    # What the leg is FOR: feed | detect | navigation | precision-landing |
    # thermal | mapping | recording. A leg may serve several.
    purpose: list[str] = Field(default_factory=list)
    # Whether the operator has this leg enabled. Metadata in v1 (the pipeline does
    # not gate on it yet); default True so existing legs are unchanged.
    enabled: bool = True
    # Who declared this leg: "operator" or a plugin id. The merge-by-owner persist
    # keys on this so an operator write preserves plugin legs and vice versa.
    # Absent → treated as operator-owned.
    owner: str | None = None
    # Horizontal field of view in degrees, when known (informational).
    fov_deg: float | None = None
    # Mount pitch offset in degrees (e.g. a 45°-down inspection cam).
    mount_pitch_deg: float | None = None
    # A calibration reference (a profile name or a stored intrinsics id).
    calibration: str | None = None
    # Physical fingerprint that re-pins source → id across a device rename. Uses
    # the wire key ``match`` (a Python keyword), aliased to the ``camera_match``
    # attribute.
    camera_match: CameraMatch | None = Field(default=None, alias="match")

    model_config = ConfigDict(populate_by_name=True)


class RecordingConfig(BaseModel):
    enabled: bool = False
    path: str = str(RECORDINGS_DIR)
    max_duration_minutes: int = 30


class UsbRecoveryConfig(BaseModel):
    """Camera USB-recovery tunables, consumed by the Rust supervisor's
    camera-recovery reconciler (it reads config.yaml directly; this model keeps
    Python from dropping the keys and documents the schema). Default-ON for
    detect + alert; destructive actions stay gated."""

    enabled: bool = True
    debounce_s: int = Field(default=20, ge=1)
    max_attempts: int = Field(default=3, ge=1)
    cooldown_schedule_s: list[int] = Field(default_factory=lambda: [10, 30, 60])
    healthy_reset_s: int = Field(default=120, ge=1)
    tick_interval_s: int = Field(default=5, ge=1)
    # Opt-in: allow a shared-hub reset (boot-time-only, guard-gated) to recover a
    # camera that failed to enumerate on a hub it shares with the radio/FC.
    allow_hub_reset: bool = False
    boot_reset_window_s: int = Field(default=180, ge=1)
    # Allow a clean per-port re-enable on an external hub that exposes per-port
    # power switching.
    allow_ppps: bool = True
    # Append usbcore.old_scheme_first=1 to the Pi boot cmdline (installer-applied)
    # as a reversible cold-boot enumeration aid. Default off (experiment).
    cold_boot_enum_aid: bool = False


class VideoConfig(BaseModel):
    mode: str = "wfb"
    wfb: WfbConfig = WfbConfig()
    camera: CameraConfig = CameraConfig()
    # Optional multi-stream list. Empty → the single ``camera`` leg (fully
    # backward compatible). Written by a driver plugin (a smart pod) via the
    # ``video.source.set`` host service, or set by an operator.
    cameras: list[CameraLeg] = Field(default_factory=list)
    recording: RecordingConfig = RecordingConfig()
    usb_recovery: UsbRecoveryConfig = UsbRecoveryConfig()
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
