"""In-process GStreamer pipeline that replaces the air-side bash chain.

The legacy air-side path composes camera capture + ffmpeg encoder +
mediamtx-air RTSP server + bash-wrapped (ffmpeg-in + python sei_injector
+ ffmpeg-RTP) into six coordinated subprocesses. Each restart cycle
risks orphan ffmpegs, stderr-parsing races, and bash-wrapper coordination
faults. This module folds the whole chain into one PyGObject-driven
GStreamer pipeline whose lifecycle is owned in-process — camera source
through RTP packetizer through ``udpsink`` straight to wfb_tx's UDP
5600 port, with an optional second ``udpsink`` branch for the cloud
relay.

Mirrors the receiver-side :class:`ados.services.video.local_tap.LocalVideoTap`
pattern: a dedicated thread owns its own ``GMainLoop``, the bus watcher
fires restart on error, and async control surfaces dispatch state-change
calls via the bus thread. The receiver's pad probe is the model for our
in-pipeline SEI injection — same UUID, same byte builder reused from
:mod:`ados.services.video.sei_injector` as a library function so there
is no second subprocess.

PyGObject is loaded via the system ``python3-gi`` apt package. When the
import fails on a rig that hasn't run ``install.sh`` yet we raise
:class:`AirPipelineUnavailable` so the caller can fall back to the
legacy bash pipeline rather than crash the video service.

Lifecycle
---------

* :meth:`AirPipeline.start` constructs the pipeline lazily, transitions
  PAUSED -> PLAYING on a daemon thread that owns its own ``GMainLoop``.
* :meth:`AirPipeline.stop` posts EOS, waits up to 1 s, then sets state
  NULL and joins the loop thread.
* :meth:`AirPipeline.stats` returns a snapshot the REST surface and
  the heartbeat enricher consume.

Watchdogs
---------

* The bus watcher restarts on ``GST_MESSAGE_ERROR`` / unexpected EOS
  with a 1/2/4/5 s capped exponential backoff. Per Rule 26, there is
  no circuit breaker — the air-side pipeline must keep retrying
  indefinitely or the radio link goes dark.
* The TX-byte watchdog polls ``/sys/class/net/<wfb_iface>/statistics/
  tx_bytes`` at 1 Hz and forces a restart when the counter stays flat
  for ≥ 15 s while the pipeline is PLAYING. Per Rule 37, process
  liveness alone is never proof of work — without this watchdog a
  wedged encoder element shows as "alive" while emitting nothing.
"""

from __future__ import annotations

import asyncio
import json
import threading
import time
from pathlib import Path
from typing import TYPE_CHECKING, Any

from ados.core.logging import get_logger
from ados.core.paths import AIR_PIPELINE_STATS_PATH as _STATS_FILE
from ados.services.video.sei_injector import (
    build_sei_nal,
    is_vcl_nal_type,
)

if TYPE_CHECKING:
    from ados.core.config import VideoConfig
    from ados.hal.camera import CameraInfo

__all__ = [
    "AirPipeline",
    "AirPipelineUnavailable",
    "AirPipelineStats",
    "build_air_pipeline_string",
    "choose_camera_source",
    "choose_encoder",
]


log = get_logger("video.air_pipeline")


# Local UDP port the wfb-ng radio reads from. wfb_tx -u 5600 listens
# here for RTP datagrams to broadcast over the radio. Identical contract
# to the legacy bash path so a flag-flip back to ffmpeg leaves the
# wfb side untouched.
_WFB_HOST = "127.0.0.1"
_WFB_PORT = 5600

# RTP packet size cap. Fits one datagram comfortably inside the
# 802.11 MTU after wfb-ng's FEC overhead. Matches the legacy ffmpeg
# `?pkt_size=1316` setting.
_RTP_MTU = 1316

# SSRC pinned for debug parity with the legacy ffmpeg flag
# `-ssrc 0xCAFE`. Lets a wireshark capture distinguish our flow from
# any other RTP stream on the host.
_RTP_SSRC = 0xCAFE

# H.264 RTP dynamic payload type. RFC 6184 doesn't reserve a number;
# 96 is the conventional dynamic baseline.
_RTP_PAYLOAD_TYPE = 96

# How long the pipeline can run silent (no advance in the kernel's
# wlan tx_bytes counter) before the TX-byte watchdog forces a restart.
# 15 s matches the legacy bash-pipeline progress watchdog; experiment
# proved smaller values false-trip during install + reload races where
# wfb_tx is being reconfigured concurrently.
_TX_SILENT_THRESHOLD_S = 15.0
_TX_POLL_INTERVAL_S = 1.0

# Restart backoff ladder. Per Rule 26 there is no give-up cap; the
# ceiling is 5 s so a transient outage recovers fast.
_RESTART_BACKOFF_LADDER_S: tuple[float, ...] = (1.0, 2.0, 4.0, 5.0, 5.0)

# How often the stats publisher thread writes the snapshot to
# ``/run/ados/air-pipeline.json``. The receiver-side ``lcd-video-tap``
# precedent uses 1 Hz; matching here keeps the GCS Hardware tab's
# refresh cadence symmetric across air + ground halves.
_STATS_PUBLISH_INTERVAL_S = 1.0


class AirPipelineUnavailable(RuntimeError):  # noqa: N818
    """Raised when the in-process GStreamer pipeline cannot run.

    Carries a short reason the caller can surface (``python3-gi``
    missing, no compatible encoder, etc.) so :func:`start_stream` can
    fall back to the legacy bash pipeline cleanly.
    """


class AirPipelineStats:
    """Mutable snapshot the AirPipeline thread updates in place.

    A plain dataclass would be just as fine; the class form makes the
    REST surface's ``response_model`` mapping a little cleaner and
    keeps every field bounded to a known type.
    """

    __slots__ = (
        "camera_source",
        "encoder_name",
        "encoder_hw_accel",
        "pipeline_state",
        "started_at",
        "last_state_change_at",
        "encoder_fps",
        "encoded_kbps",
        "sei_injected_count",
        "udp_bytes_out",
        "last_buffer_at",
        "restart_count",
        "tx_silent_kicks",
        "bus_errors",
        "cloud_branch_open",
    )

    def __init__(self) -> None:
        self.camera_source: str = ""
        self.encoder_name: str = ""
        self.encoder_hw_accel: bool = False
        self.pipeline_state: str = "idle"
        self.started_at: float | None = None
        self.last_state_change_at: float | None = None
        self.encoder_fps: float = 0.0
        self.encoded_kbps: float = 0.0
        self.sei_injected_count: int = 0
        self.udp_bytes_out: int = 0
        self.last_buffer_at: float | None = None
        self.restart_count: int = 0
        self.tx_silent_kicks: int = 0
        self.bus_errors: int = 0
        self.cloud_branch_open: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "camera_source": self.camera_source,
            "encoder_name": self.encoder_name,
            "encoder_hw_accel": self.encoder_hw_accel,
            "pipeline_state": self.pipeline_state,
            "started_at": self.started_at,
            "last_state_change_at": self.last_state_change_at,
            "encoder_fps": round(self.encoder_fps, 2),
            "encoded_kbps": round(self.encoded_kbps, 1),
            "sei_injected_count": int(self.sei_injected_count),
            "udp_bytes_out": int(self.udp_bytes_out),
            "last_buffer_at": self.last_buffer_at,
            "restart_count": int(self.restart_count),
            "tx_silent_kicks": int(self.tx_silent_kicks),
            "bus_errors": int(self.bus_errors),
            "cloud_branch_open": bool(self.cloud_branch_open),
        }


def _gst_element_available(name: str) -> bool:
    """Return True when the named element factory can be instantiated.

    Cheap probe used during tier selection: a missing plugin returns
    False without raising so the next tier can be tried. Returns False
    when GStreamer itself is not importable, which is the right
    semantic for "no element available."
    """
    try:
        import gi  # noqa: F401

        gi.require_version("Gst", "1.0")
        from gi.repository import Gst
    except (ImportError, ValueError):
        return False
    if not Gst.is_initialized():
        try:
            Gst.init(None)
        except Exception:  # noqa: BLE001
            return False
    try:
        factory = Gst.ElementFactory.find(name)
    except Exception:  # noqa: BLE001
        return False
    return factory is not None


def choose_camera_source(
    camera: CameraInfo | None,
    *,
    soc: str,
    width: int,
    height: int,
    fps: int,
) -> tuple[str, str]:
    """Pick the GStreamer source element for the camera + SoC combo.

    Returns ``(element_string, source_kind)``. ``element_string`` is the
    leading slice of the pipeline up to (and including) the trailing
    ``! `` token, with caps already applied where needed.
    ``source_kind`` is a short identifier the stats snapshot can echo
    back to the GCS ("libcamerasrc", "v4l2src", "rpicamsrc",
    "videotestsrc"). The output is always raw video (NV12 or YUY2)
    ready to feed an encoder.

    Selection order:

    1. CSI camera on Rockchip/Allwinner SBC -> ``libcamerasrc`` if the
       element is available. The Rockchip libcamera stack is the
       supported path on Rock 5C Lite + Cubie A7Z.
    2. CSI on Raspberry Pi -> ``libcamerasrc`` (Bookworm) when
       available; fall back to ``rpicamsrc`` (legacy MMAL stack)
       otherwise. libcamerasrc is preferred because the rpicamsrc
       plugin is being deprecated on newer Raspberry Pi OS releases.
    3. USB UVC camera -> ``v4l2src device=<path>`` with explicit
       caps. Works on every platform with a UVC kernel driver.
    4. No camera / dev host -> ``videotestsrc`` so unit tests can
       assemble the pipeline string without real hardware.
    """
    soc_lower = (soc or "").lower()
    fps_int = max(1, int(fps))
    width_int = max(1, int(width))
    height_int = max(1, int(height))

    if camera is None:
        # Dev / CI fallback. Stripes pattern at the requested geometry.
        return (
            "videotestsrc is-live=true pattern=smpte "
            f"! video/x-raw,width={width_int},height={height_int},"
            f"framerate={fps_int}/1,format=I420 ",
            "videotestsrc",
        )

    # CameraType is a StrEnum; compare via the string value to keep this
    # function importable without bringing in the camera module.
    camera_kind = str(getattr(getattr(camera, "type", ""), "value", "") or "")
    device_path = getattr(camera, "device_path", "") or ""

    if camera_kind == "csi":
        if _gst_element_available("libcamerasrc"):
            return (
                "libcamerasrc "
                f"! video/x-raw,width={width_int},height={height_int},"
                f"framerate={fps_int}/1,format=NV12 ",
                "libcamerasrc",
            )
        if soc_lower.startswith("bcm271") and _gst_element_available(
            "rpicamsrc"
        ):
            # Legacy MMAL path on older Pi OS. ``rpicamsrc`` exposes
            # encoder controls directly, so we ask for raw and let the
            # downstream encoder handle bitrate.
            return (
                "rpicamsrc preview=false "
                f"! video/x-raw,width={width_int},height={height_int},"
                f"framerate={fps_int}/1 ",
                "rpicamsrc",
            )

    if camera_kind == "usb" and device_path:
        # USB UVC. ``YUY2`` is the most universal capture format; some
        # cheap UVC cameras only emit MJPEG, which we ignore here for
        # v1 — software encode from MJPEG-decoded raw is the Phase 14
        # consideration.
        return (
            f"v4l2src device={device_path} "
            f"! video/x-raw,width={width_int},height={height_int},"
            f"framerate={fps_int}/1,format=YUY2 "
            "! videoconvert ! video/x-raw,format=I420 ",
            "v4l2src",
        )

    if camera_kind == "ip" and device_path:
        # IP camera. Pull the H.264 over RTSP and let the encoder tier
        # detect it as already-encoded so we skip the libx264 hop.
        return (
            f"rtspsrc location={device_path} protocols=tcp "
            "! rtph264depay ! video/x-h264,stream-format=byte-stream ",
            "rtspsrc_passthrough",
        )

    # Anything else (or a CSI camera with no libcamera element) falls
    # back to the test source so the pipeline builds. A loud warning
    # surface this on the journal so the operator notices the missing
    # plugin instead of seeing test stripes silently shipped over the
    # radio.
    log.warning(
        "air_pipeline_camera_source_fallback",
        camera_kind=camera_kind,
        device_path=device_path,
        soc=soc,
    )
    return (
        "videotestsrc is-live=true pattern=ball "
        f"! video/x-raw,width={width_int},height={height_int},"
        f"framerate={fps_int}/1,format=I420 ",
        "videotestsrc_fallback",
    )


def choose_encoder(
    *,
    soc: str,
    hw_video_codecs: list[str] | None,
    bitrate_kbps: int,
    keyframe_interval: int,
    prefer_hw: bool = True,
) -> tuple[str, str, bool]:
    """Pick the H.264 encoder element + GStreamer launch substring.

    Returns ``(element_string, encoder_name, hw_accel)``. The launch
    substring includes any encoder-specific properties (bitrate,
    keyframe cadence, tune flags, level/profile) plus the trailing
    ``! `` separator and a caps filter pinning byte-stream + AU
    alignment so h264parse downstream doesn't have to re-negotiate.

    Tier order, with ``prefer_hw=True`` (default):

    1. **Hardware (best-effort per board profile)**:
       - Pi family with v4l2h264enc: bitrate + keyframe + level via
         ``extra-controls``.
       - Rockchip family with mpph264enc: bitrate, gop, profile=baseline.
         Known fragile on RK3582/RV1106; falls through to Tier 3 when
         caps negotiation eventually fails at PLAYING.
       - NVIDIA Jetson with nvv4l2h264enc / omxh264enc.
    2. **Software (fallback, always available where libx264 is
       installed)**: ``x264enc`` with ``speed-preset=ultrafast
       tune=zerolatency bframes=0 key-int-max=N bitrate=<kbps>``.

    ``hw_video_codecs`` is the matching board profile's allow-list of
    codec capabilities (e.g. ``["h264_enc", "h264_dec", "h265_dec"]``).
    Without the ``h264_enc`` capability we skip straight to software
    regardless of element presence — the kernel driver or DTS for the
    hardware encoder is not wired on this board.
    """
    codecs = set(hw_video_codecs or [])
    soc_lower = (soc or "").lower()
    bitrate = max(64, int(bitrate_kbps))
    gop = max(1, int(keyframe_interval))

    if prefer_hw and "h264_enc" in codecs:
        # Raspberry Pi V4L2 mem2mem H.264 encoder. Available on Pi 4B
        # (Broadcom VideoCore) and on Pi 5 + Pi Zero 2 W via the
        # bcm2835-codec driver.
        if soc_lower.startswith("bcm271") and _gst_element_available(
            "v4l2h264enc"
        ):
            bps = bitrate * 1000
            return (
                f"v4l2h264enc extra-controls=\"controls,"
                f"h264_profile=1,h264_level=11,"
                f"h264_i_frame_period={gop},"
                f"video_bitrate={bps}\" "
                "! video/x-h264,profile=baseline,"
                "stream-format=byte-stream,alignment=au ",
                "v4l2h264enc",
                True,
            )
        # Rockchip MPP. Known fragile on RK3582/RV1106 — bench shows the
        # element parses fine but caps negotiation can fail at PLAYING.
        # We still try it because on Rock 5B / RK3576 it works.
        if (
            soc_lower.startswith("rk")
            and _gst_element_available("mpph264enc")
        ):
            return (
                f"mpph264enc bps={bitrate * 1000} gop={gop} "
                "profile=baseline rc-mode=cbr "
                "! video/x-h264,stream-format=byte-stream,alignment=au ",
                "mpph264enc",
                True,
            )
        # NVIDIA Jetson: nvv4l2h264enc is on Orin Nano, omxh264enc on
        # legacy Nano boards. Both expose `bitrate` and `iframeinterval`.
        if "nvidia" in soc_lower or "tegra" in soc_lower or "jetson" in soc_lower:
            if _gst_element_available("nvv4l2h264enc"):
                return (
                    f"nvv4l2h264enc bitrate={bitrate * 1000} "
                    f"iframeinterval={gop} insert-sps-pps=true "
                    "control-rate=1 "
                    "! video/x-h264,stream-format=byte-stream,alignment=au ",
                    "nvv4l2h264enc",
                    True,
                )
            if _gst_element_available("omxh264enc"):
                return (
                    f"omxh264enc bitrate={bitrate * 1000} "
                    f"iframeinterval={gop} insert-sps-pps=true "
                    "control-rate=2 "
                    "! video/x-h264,stream-format=byte-stream,alignment=au ",
                    "omxh264enc",
                    True,
                )

    # Software fallback. Always available where libx264 is installed.
    if not _gst_element_available("x264enc"):
        raise AirPipelineUnavailable(
            "no GStreamer H.264 encoder found (need v4l2h264enc, "
            "mpph264enc, nvv4l2h264enc, omxh264enc, or x264enc)"
        )
    return (
        f"x264enc speed-preset=ultrafast tune=zerolatency "
        f"threads=2 sliced-threads=false bframes=0 "
        f"key-int-max={gop} bitrate={bitrate} "
        "! video/x-h264,profile=baseline,"
        "stream-format=byte-stream,alignment=au ",
        "x264enc",
        False,
    )


def build_air_pipeline_string(
    *,
    camera: CameraInfo | None,
    soc: str,
    hw_video_codecs: list[str] | None,
    width: int,
    height: int,
    fps: int,
    bitrate_kbps: int,
    keyframe_interval: int,
    cloud_branch_enabled: bool,
    cloud_rtp_port: int,
    prefer_hw_encoder: bool = True,
) -> tuple[str, dict[str, Any]]:
    """Compose the full gst-launch-style pipeline string.

    Returns ``(pipeline_string, metadata)``. The metadata dict carries
    the camera kind, encoder name, and hardware-accel flag the stats
    snapshot needs.

    Topology::

        <camera source> ! <encoder> ! h264parse name=h264parse_air
          ! rtph264pay name=rtph264pay_air config-interval=1 mtu=1316 pt=96 ssrc=0xCAFE
          ! tee name=t allow-not-linked=true
            t. ! queue leaky=downstream max-size-buffers=8
                 ! udpsink name=wfb_sink host=127.0.0.1 port=5600 sync=false async=false
            t. ! queue leaky=downstream max-size-buffers=8
                 ! identity name=cloud_gate drop-buffers=<bool>
                 ! udpsink name=cloud_sink host=127.0.0.1
                       port=<cloud_rtp_port> sync=false async=false

    The cloud branch's ``identity`` element flips between
    ``drop-buffers=true`` (cloud relay off) and ``drop-buffers=false``
    (cloud relay on) at runtime. ``allow-not-linked=true`` on the tee
    lets a transient teardown of either branch not bring the pipeline
    down with it.
    """
    camera_str, camera_kind = choose_camera_source(
        camera,
        soc=soc,
        width=width,
        height=height,
        fps=fps,
    )

    encoder_str: str
    encoder_name: str
    hw_accel: bool
    if camera_kind == "rtspsrc_passthrough":
        # Already-encoded H.264 from an IP camera. Skip the encoder and
        # just parse + payload + sink. The receiver still gets SEI via
        # the pad probe, which is the desired behavior.
        encoder_str = ""
        encoder_name = "passthrough"
        hw_accel = False
    else:
        encoder_str, encoder_name, hw_accel = choose_encoder(
            soc=soc,
            hw_video_codecs=hw_video_codecs,
            bitrate_kbps=bitrate_kbps,
            keyframe_interval=keyframe_interval,
            prefer_hw=prefer_hw_encoder,
        )

    # ``identity drop-buffers=true`` is the runtime gate for the cloud
    # branch. Even when we know cloud is off at construction time we
    # always wire the branch so the runtime flip is a single property
    # set; ``allow-not-linked=true`` keeps the tee happy if the cloud
    # leg ever goes unlinked at teardown.
    cloud_drop = "false" if cloud_branch_enabled else "true"

    pipeline_str = (
        f"{camera_str}"
        f"{encoder_str}"
        "! h264parse name=h264parse_air "
        f"! rtph264pay name=rtph264pay_air config-interval=1 "
        f"mtu={_RTP_MTU} pt={_RTP_PAYLOAD_TYPE} ssrc={_RTP_SSRC} "
        "! tee name=t allow-not-linked=true "
        "t. ! queue leaky=downstream "
        "max-size-buffers=8 max-size-time=0 max-size-bytes=0 "
        f"! udpsink name=wfb_sink host={_WFB_HOST} port={_WFB_PORT} "
        "sync=false async=false "
        "t. ! queue leaky=downstream "
        "max-size-buffers=8 max-size-time=0 max-size-bytes=0 "
        f"! identity name=cloud_gate drop-buffers={cloud_drop} "
        f"! udpsink name=cloud_sink host={_WFB_HOST} port={int(cloud_rtp_port)} "
        "sync=false async=false"
    )
    metadata = {
        "camera_source": camera_kind,
        "encoder_name": encoder_name,
        "encoder_hw_accel": hw_accel,
        "cloud_branch_enabled": bool(cloud_branch_enabled),
    }
    return pipeline_str, metadata


def _resolve_wfb_iface() -> str | None:
    """Return the monitor-mode wlan iface used by wfb_tx, if discoverable.

    Reads ``/etc/ados/config.yaml`` non-fatally to fish out
    ``video.wfb.interface``. Tests stub this out, so the function
    silently returns None on any failure rather than throwing.
    """
    try:
        import yaml as _yaml

        from ados.core.paths import CONFIG_YAML

        if not Path(CONFIG_YAML).exists():
            return None
        with open(CONFIG_YAML) as handle:
            data = _yaml.safe_load(handle) or {}
    except Exception:  # noqa: BLE001
        return None
    try:
        iface = data.get("video", {}).get("wfb", {}).get("interface", "") or ""
    except AttributeError:
        return None
    iface = str(iface).strip()
    return iface or None


def _read_tx_bytes(iface: str) -> int | None:
    """Return the kernel's tx_bytes counter for ``iface`` or None on miss."""
    path = Path(f"/sys/class/net/{iface}/statistics/tx_bytes")
    try:
        return int(path.read_text().strip())
    except (OSError, ValueError):
        return None


class AirPipeline:
    """In-process GStreamer pipeline driving the wfb_tx feed.

    All state-change calls are exposed as ``async`` methods so the
    asyncio video service can await them without blocking. The
    pipeline itself runs on a dedicated daemon thread with its own
    ``GMainLoop``; PyGObject callbacks fire on that thread.
    """

    def __init__(
        self,
        *,
        video_config: VideoConfig,
        camera: CameraInfo | None,
        board_soc: str,
        board_hw_codecs: list[str] | None,
        cloud_relay_enabled: bool,
        sei_latency_enabled: bool,
        logger: Any | None = None,
    ) -> None:
        self._config = video_config
        self._camera = camera
        self._board_soc = board_soc
        self._board_hw_codecs = list(board_hw_codecs or [])
        self._cloud_relay_enabled = bool(cloud_relay_enabled)
        self._sei_latency_enabled = bool(sei_latency_enabled)
        self._logger = logger or log

        self._stats = AirPipelineStats()
        self._stats.cloud_branch_open = self._cloud_relay_enabled
        # Restart bookkeeping.
        self._consecutive_restart_failures: int = 0
        # Lazily-bound PyGObject objects. Held as ``Any`` so the type
        # checker doesn't need PyGObject in CI.
        self._Gst: Any | None = None
        self._GLib: Any | None = None
        self._pipeline: Any | None = None
        self._h264parse: Any | None = None
        self._h264parse_probe_id: int | None = None
        self._wfb_sink: Any | None = None
        self._cloud_gate: Any | None = None
        self._cloud_sink: Any | None = None
        self._loop: Any | None = None
        self._thread: threading.Thread | None = None
        self._stop_requested = threading.Event()
        self._lock = threading.Lock()
        self._tx_watchdog_thread: threading.Thread | None = None
        self._stats_publisher_thread: threading.Thread | None = None
        self._tx_last_advance_at: float = 0.0
        self._tx_last_bytes_seen: int | None = None
        self._wfb_iface: str | None = None
        # Public pipeline launch string + chosen metadata. Set in
        # ``start()``; exposed via ``stats()`` so tests / GCS can verify
        # what was actually launched.
        self._pipeline_str: str = ""

    # ── public API ─────────────────────────────────────────────

    async def start(self) -> None:
        """Construct the pipeline and transition to PLAYING.

        Raises :class:`AirPipelineUnavailable` when PyGObject isn't
        importable or no compatible encoder element is available.
        """
        with self._lock:
            if self._pipeline is not None:
                return
            try:
                import gi
            except ImportError as exc:
                raise AirPipelineUnavailable(
                    "python3-gi or gstreamer not installed"
                ) from exc
            try:
                gi.require_version("Gst", "1.0")
                from gi.repository import GLib, Gst
            except (ImportError, ValueError) as exc:
                raise AirPipelineUnavailable(
                    "gstreamer-1.0 typelib not available"
                ) from exc
            if not Gst.is_initialized():
                Gst.init(None)
            self._Gst = Gst
            self._GLib = GLib

            cam = self._config.camera
            pipeline_str, meta = build_air_pipeline_string(
                camera=self._camera,
                soc=self._board_soc,
                hw_video_codecs=self._board_hw_codecs,
                width=cam.width,
                height=cam.height,
                fps=cam.fps,
                bitrate_kbps=cam.bitrate_kbps,
                # Encoder keyframe interval in frames. A 1-second GOP
                # at the camera fps trades a bit of bandwidth for fast
                # late-joiner recovery and resync after a wfb-ng FEC
                # block loss. Matches the OpenHD reference's GOP
                # budget on the same class of hardware.
                keyframe_interval=max(1, int(cam.fps)),
                cloud_branch_enabled=self._cloud_relay_enabled,
                cloud_rtp_port=int(self._config.cloud_rtp_port),
                prefer_hw_encoder=bool(self._config.prefer_hw_encoder),
            )
            self._pipeline_str = pipeline_str
            self._stats.camera_source = str(meta.get("camera_source", ""))
            self._stats.encoder_name = str(meta.get("encoder_name", ""))
            self._stats.encoder_hw_accel = bool(meta.get("encoder_hw_accel"))

            self._logger.info(
                "air_pipeline_pipeline_constructed",
                camera=self._stats.camera_source,
                encoder=self._stats.encoder_name,
                hw_accel=self._stats.encoder_hw_accel,
                cloud_branch=self._cloud_relay_enabled,
                sei_enabled=self._sei_latency_enabled,
            )

            try:
                pipeline = Gst.parse_launch(pipeline_str)
            except Exception as exc:  # noqa: BLE001
                raise AirPipelineUnavailable(
                    f"gstreamer pipeline parse failed: {exc}"
                ) from exc

            h264parse = pipeline.get_by_name("h264parse_air")
            wfb_sink = pipeline.get_by_name("wfb_sink")
            cloud_gate = pipeline.get_by_name("cloud_gate")
            cloud_sink = pipeline.get_by_name("cloud_sink")
            if (
                h264parse is None
                or wfb_sink is None
                or cloud_gate is None
                or cloud_sink is None
            ):
                pipeline.set_state(Gst.State.NULL)
                raise AirPipelineUnavailable(
                    "pipeline missing expected named elements "
                    "(h264parse_air / wfb_sink / cloud_gate / cloud_sink)"
                )

            # Pad probe on h264parse src for SEI injection. Mirror of
            # the receiver-side probe in local_tap; the byte builder
            # lives in sei_injector for byte-level symmetry.
            if self._sei_latency_enabled:
                src_pad = h264parse.get_static_pad("src")
                if src_pad is None:
                    self._logger.warning(
                        "air_pipeline_h264parse_no_src_pad",
                        note="SEI markers will not be injected",
                    )
                else:
                    try:
                        probe_mask = Gst.PadProbeType.BUFFER
                        probe_id = src_pad.add_probe(
                            probe_mask, self._on_h264_buffer, None,
                        )
                        self._h264parse_probe_id = probe_id
                        self._logger.info("air_pipeline_sei_probe_installed")
                    except Exception as exc:  # noqa: BLE001
                        self._logger.warning(
                            "air_pipeline_sei_probe_install_failed",
                            error=str(exc),
                        )
            self._h264parse = h264parse
            self._wfb_sink = wfb_sink
            self._cloud_gate = cloud_gate
            self._cloud_sink = cloud_sink

            bus = pipeline.get_bus()
            bus.add_signal_watch()
            bus.connect("message", self._on_bus_message)

            self._pipeline = pipeline
            self._stop_requested.clear()

            ret = pipeline.set_state(Gst.State.PAUSED)
            if ret == Gst.StateChangeReturn.FAILURE:
                pipeline.set_state(Gst.State.NULL)
                self._pipeline = None
                raise AirPipelineUnavailable(
                    "gstreamer pipeline refused to PAUSED"
                )
            self._stats.pipeline_state = "paused"
            self._stats.last_state_change_at = time.monotonic()

            self._thread = threading.Thread(
                target=self._run_loop_thread,
                name="ados-air-pipeline",
                daemon=True,
            )
            self._thread.start()

            # TX-byte counter watchdog (Rule 37) — gates on the wlan
            # interface advancing. The wfb iface is resolved lazily so
            # a config edit after start_stream is picked up on the
            # next watchdog iteration.
            self._wfb_iface = _resolve_wfb_iface()
            self._tx_last_advance_at = time.monotonic()
            self._tx_last_bytes_seen = None
            self._tx_watchdog_thread = threading.Thread(
                target=self._tx_byte_watchdog,
                name="ados-air-pipeline-watchdog",
                daemon=True,
            )
            self._tx_watchdog_thread.start()

            # Stats publisher. Writes _STATS_FILE at 1 Hz for the REST
            # surface + heartbeat enricher to consume.
            self._stats_publisher_thread = threading.Thread(
                target=self._stats_publisher,
                name="ados-air-pipeline-stats",
                daemon=True,
            )
            self._stats_publisher_thread.start()

            self._stats.started_at = time.monotonic()

    async def stop(self) -> None:
        """Tear down the pipeline cleanly."""
        pipeline = self._pipeline
        loop = self._loop
        thread = self._thread
        gst = self._Gst
        if pipeline is None:
            self._reset_stats()
            return
        self._stop_requested.set()
        # Remove SEI probe first so the callback can't fire on a
        # half-torn-down pipeline.
        h264parse = self._h264parse
        probe_id = self._h264parse_probe_id
        if h264parse is not None and probe_id is not None:
            try:
                src_pad = h264parse.get_static_pad("src")
                if src_pad is not None:
                    src_pad.remove_probe(probe_id)
            except Exception as exc:  # noqa: BLE001
                self._logger.debug(
                    "air_pipeline_sei_probe_remove_failed", error=str(exc)
                )
        try:
            if gst is not None:
                pipeline.send_event(gst.Event.new_eos())
        except Exception as exc:  # noqa: BLE001
            self._logger.debug("air_pipeline_eos_failed", error=str(exc))
        await asyncio.sleep(0.1)
        try:
            if gst is not None:
                pipeline.set_state(gst.State.NULL)
        except Exception as exc:  # noqa: BLE001
            self._logger.debug("air_pipeline_null_failed", error=str(exc))
        if loop is not None:
            try:
                loop.quit()
            except Exception as exc:  # noqa: BLE001
                self._logger.debug(
                    "air_pipeline_loop_quit_failed", error=str(exc)
                )
        if thread is not None:
            thread.join(timeout=1.0)
        with self._lock:
            self._pipeline = None
            self._h264parse = None
            self._h264parse_probe_id = None
            self._wfb_sink = None
            self._cloud_gate = None
            self._cloud_sink = None
            self._loop = None
            self._thread = None
            self._stats.pipeline_state = "stopped"
        self._reset_stats()
        self._logger.info("air_pipeline_stopped")

    def stats(self) -> dict[str, Any]:
        """Snapshot of pipeline telemetry."""
        return self._stats.to_dict()

    def is_running(self) -> bool:
        """Cheap liveness check for the supervisor health probe."""
        return self._pipeline is not None and self._stats.pipeline_state in (
            "playing",
            "paused",
        )

    async def set_bitrate(self, kbps: int) -> None:
        """Live-tune the encoder's bitrate.

        Best-effort: silently no-op if the chosen encoder element does
        not expose the property. Restarting the pipeline is the safe
        fallback for an unsupported encoder.
        """
        if self._pipeline is None:
            return
        encoder = self._pipeline.get_by_name("h264enc")  # legacy name
        if encoder is None:
            return
        try:
            encoder.set_property("bitrate", max(64, int(kbps)))
        except Exception as exc:  # noqa: BLE001
            self._logger.debug("air_pipeline_set_bitrate_failed", error=str(exc))

    async def set_cloud_branch(self, *, open_branch: bool) -> None:
        """Open or close the cloud relay branch at runtime."""
        gate = self._cloud_gate
        if gate is None:
            return
        try:
            gate.set_property("drop-buffers", not bool(open_branch))
            self._stats.cloud_branch_open = bool(open_branch)
            self._logger.info(
                "air_pipeline_cloud_branch_toggled",
                open=bool(open_branch),
            )
        except Exception as exc:  # noqa: BLE001
            self._logger.warning(
                "air_pipeline_cloud_branch_toggle_failed", error=str(exc)
            )

    # ── internals ──────────────────────────────────────────────

    def _reset_stats(self) -> None:
        self._stats.encoder_fps = 0.0
        self._stats.encoded_kbps = 0.0
        self._stats.last_buffer_at = None
        self._stats.sei_injected_count = 0
        # Restart + error counters are deliberately preserved so the
        # operator can see history; only the per-run rates reset.

    def _on_h264_buffer(self, _pad: Any, info: Any, _user: Any) -> int:
        """Pad-probe callback. Injects SEI before each VCL slice.

        Runs on the GStreamer streaming thread. The receiver's parser
        is byte-level tolerant; we only need to splice a SEI NAL in
        front of the first VCL slice in each access unit.
        """
        gst = self._Gst
        if gst is None:
            return 1  # Gst.PadProbeReturn.OK
        try:
            buf = info.get_buffer()
        except Exception:  # noqa: BLE001
            return 1
        if buf is None:
            return 1
        success, mapinfo = buf.map(gst.MapFlags.READ)
        if not success:
            return 1
        try:
            data = bytes(mapinfo.data)
        finally:
            buf.unmap(mapinfo)
        if not data:
            return 1
        modified = self._inject_sei_into_au(data)
        if modified is data:
            return 1
        # Replace the buffer's bytes via a fresh buffer carrying the
        # spliced stream. We can't mutate the existing buffer (it may
        # be ref-shared upstream); instead we allocate, copy, and use
        # gst_buffer_replace to swap the payload in place.
        try:
            new_buf = gst.Buffer.new_wrapped(modified)
            # Preserve timestamps so downstream timing is unchanged.
            new_buf.pts = buf.pts
            new_buf.dts = buf.dts
            new_buf.duration = buf.duration
            new_buf.offset = buf.offset
            new_buf.offset_end = buf.offset_end
            # PadProbeInfo offers data assignment on PyGObject 3.x; the
            # exact attribute name shifts across releases. Try the
            # common shapes and fall back to a debug log if none work.
            for attr in ("data", "buffer"):
                if hasattr(info, attr):
                    try:
                        setattr(info, attr, new_buf)
                        self._stats.sei_injected_count += 1
                        return 1
                    except Exception:  # noqa: BLE001
                        continue
            # No mutable slot found — log once per N misses and let the
            # original buffer pass through unchanged.
            self._logger.debug(
                "air_pipeline_sei_probe_info_immutable",
                note="probe info has no writable buffer slot",
            )
        except Exception as exc:  # noqa: BLE001
            self._logger.debug(
                "air_pipeline_sei_splice_failed", error=str(exc)
            )
        return 1

    def _inject_sei_into_au(self, data: bytes) -> bytes:
        """Return ``data`` with a SEI NAL prefixed before the first VCL slice.

        Walks the Annex-B byte stream once, splices a fresh
        :func:`build_sei_nal(time.time_ns())` in front of the first
        VCL slice it sees. Returns the original ``data`` reference
        when no VCL slice is found (SPS/PPS-only buffer, etc.).
        """
        n = len(data)
        if n < 4:
            return data
        i = 0
        while i + 2 < n:
            sc_len = 0
            if (
                i + 4 <= n
                and data[i] == 0
                and data[i + 1] == 0
                and data[i + 2] == 0
                and data[i + 3] == 1
            ):
                sc_len = 4
            elif (
                i + 3 <= n
                and data[i] == 0
                and data[i + 1] == 0
                and data[i + 2] == 1
            ):
                sc_len = 3
            if sc_len == 0:
                i += 1
                continue
            nal_byte_idx = i + sc_len
            if nal_byte_idx >= n:
                break
            nal_byte = data[nal_byte_idx]
            if is_vcl_nal_type(nal_byte):
                sei = build_sei_nal(time.time_ns())
                return data[:i] + sei + data[i:]
            i = nal_byte_idx + 1
        return data

    def _on_bus_message(self, _bus: Any, message: Any) -> bool:
        """Handle ERROR / EOS / WARNING bus posts and trigger restart."""
        gst = self._Gst
        if gst is None:
            return True
        msg_type = message.type
        if msg_type == gst.MessageType.EOS:
            self._logger.info("air_pipeline_eos")
            self._stats.pipeline_state = "eos"
            self._maybe_auto_restart(reason="eos")
        elif msg_type == gst.MessageType.ERROR:
            err, dbg = message.parse_error()
            self._logger.warning(
                "air_pipeline_bus_error",
                error=str(err),
                debug=str(dbg),
            )
            self._stats.pipeline_state = "error"
            self._stats.bus_errors += 1
            self._maybe_auto_restart(reason="bus_error")
        elif msg_type == gst.MessageType.STATE_CHANGED:
            if message.src is self._pipeline:
                _old, new, _pending = message.parse_state_changed()
                try:
                    name = new.value_nick
                except Exception:  # noqa: BLE001
                    name = str(new)
                if name and name != self._stats.pipeline_state:
                    self._stats.pipeline_state = name
                    self._stats.last_state_change_at = time.monotonic()
                    self._logger.info(
                        "air_pipeline_state_changed", state=name
                    )
        elif msg_type == gst.MessageType.WARNING:
            warn, dbg = message.parse_warning()
            self._logger.debug(
                "air_pipeline_bus_warning",
                warning=str(warn),
                debug=str(dbg),
            )
        return True

    def _run_loop_thread(self) -> None:
        glib = self._GLib
        gst = self._Gst
        pipeline = self._pipeline
        if glib is None or gst is None or pipeline is None:
            return
        loop = glib.MainLoop()
        self._loop = loop
        try:
            pipeline.set_state(gst.State.PLAYING)
            self._stats.pipeline_state = "playing"
            self._stats.last_state_change_at = time.monotonic()
            self._logger.info("air_pipeline_started")
            loop.run()
        except Exception as exc:  # noqa: BLE001
            self._logger.warning(
                "air_pipeline_loop_crashed", error=str(exc)
            )
        finally:
            try:
                pipeline.set_state(gst.State.NULL)
            except Exception:  # noqa: BLE001
                pass

    def _maybe_auto_restart(self, *, reason: str) -> None:
        gst = self._Gst
        pipeline = self._pipeline
        if gst is None or pipeline is None or self._stop_requested.is_set():
            return
        self._consecutive_restart_failures += 1
        ladder_idx = min(
            self._consecutive_restart_failures - 1,
            len(_RESTART_BACKOFF_LADDER_S) - 1,
        )
        delay = _RESTART_BACKOFF_LADDER_S[ladder_idx]
        self._logger.info(
            "air_pipeline_restart_scheduled",
            reason=reason,
            delay_s=delay,
            attempt=self._consecutive_restart_failures,
        )
        self._stats.restart_count = max(
            self._stats.restart_count, self._consecutive_restart_failures
        )
        threading.Timer(
            delay,
            self._do_restart,
            kwargs={"reason": reason},
        ).start()

    def _do_restart(self, *, reason: str) -> None:
        gst = self._Gst
        pipeline = self._pipeline
        if gst is None or pipeline is None or self._stop_requested.is_set():
            return
        try:
            pipeline.set_state(gst.State.NULL)
            try:
                pipeline.get_state(1_000_000_000)
            except Exception:  # noqa: BLE001
                pass
            pipeline.set_state(gst.State.PLAYING)
            self._stats.pipeline_state = "playing"
            self._stats.last_state_change_at = time.monotonic()
            self._logger.info("air_pipeline_auto_restart", reason=reason)
            # On a successful PLAYING transition, clear the consecutive
            # counter so the next stretch of healthy frames does not
            # tick into the back of the ladder.
            self._consecutive_restart_failures = 0
        except Exception as exc:  # noqa: BLE001
            self._logger.warning(
                "air_pipeline_restart_failed", error=str(exc)
            )
            self._stats.pipeline_state = "error"

    def _tx_byte_watchdog(self) -> None:
        """Force a restart when the wlan tx_bytes counter stays flat.

        Rule 37: process liveness alone is never proof of work. A
        wedged encoder element keeps the pipeline in PLAYING while
        emitting zero bytes. The kernel's per-interface counter is the
        authoritative ground truth.
        """
        while not self._stop_requested.is_set():
            try:
                time.sleep(_TX_POLL_INTERVAL_S)
            except Exception:  # noqa: BLE001
                return
            if self._stop_requested.is_set():
                return
            if self._stats.pipeline_state != "playing":
                # Reset the silence timer whenever we're not in PLAYING
                # so a paused/idle window doesn't pre-arm a false kick.
                self._tx_last_advance_at = time.monotonic()
                self._tx_last_bytes_seen = None
                continue
            iface = self._wfb_iface
            if iface is None:
                # Re-resolve lazily; config might have landed late.
                iface = _resolve_wfb_iface()
                self._wfb_iface = iface
                if iface is None:
                    continue
            bytes_now = _read_tx_bytes(iface)
            if bytes_now is None:
                continue
            last = self._tx_last_bytes_seen
            if last is None or bytes_now > last:
                self._tx_last_advance_at = time.monotonic()
                self._tx_last_bytes_seen = bytes_now
                self._stats.udp_bytes_out = bytes_now
                self._stats.last_buffer_at = self._tx_last_advance_at
                continue
            silent_for = time.monotonic() - self._tx_last_advance_at
            if silent_for < _TX_SILENT_THRESHOLD_S:
                continue
            self._logger.warning(
                "air_pipeline_tx_silent",
                silent_s=round(silent_for, 1),
                threshold_s=_TX_SILENT_THRESHOLD_S,
                iface=iface,
                note="kernel tx_bytes flat while PLAYING; forcing restart",
            )
            self._stats.tx_silent_kicks += 1
            try:
                self._maybe_auto_restart(reason="tx_silent")
            except Exception as exc:  # noqa: BLE001
                self._logger.warning(
                    "air_pipeline_watchdog_restart_failed", error=str(exc)
                )
            # Re-arm so we don't immediately re-trigger before the
            # restart lands.
            self._tx_last_advance_at = time.monotonic()
            self._tx_last_bytes_seen = None

    def _stats_publisher(self) -> None:
        """Write the stats snapshot to _STATS_FILE at 1 Hz."""
        run_dir = _STATS_FILE.parent
        try:
            run_dir.mkdir(parents=True, exist_ok=True)
        except OSError:
            # /run/ados is created by tmpfiles.d on a real rig; on a
            # dev host without /run/ados the publisher silently
            # disables itself.
            return
        while not self._stop_requested.is_set():
            try:
                time.sleep(_STATS_PUBLISH_INTERVAL_S)
            except Exception:  # noqa: BLE001
                return
            if self._stop_requested.is_set():
                return
            try:
                snapshot = self._stats.to_dict()
                snapshot["updated_at_ms"] = int(time.time() * 1000)
                tmp = _STATS_FILE.with_suffix(".tmp")
                tmp.write_text(json.dumps(snapshot))
                tmp.replace(_STATS_FILE)
            except Exception as exc:  # noqa: BLE001
                self._logger.debug(
                    "air_pipeline_stats_publish_failed", error=str(exc)
                )
