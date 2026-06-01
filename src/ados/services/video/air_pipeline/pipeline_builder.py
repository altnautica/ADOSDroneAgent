"""Pure-function helpers that compose the air-side GStreamer pipeline.

The :class:`AirPipeline` class composes :func:`build_air_pipeline_string`
plus the camera-source and encoder selectors to produce the launch
string GStreamer parses. Splitting the helpers out lets bench operators
(and unit tests) regenerate the exact pipeline string for a given
SoC + camera combination without instantiating the full pipeline.
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import TYPE_CHECKING, Any

from ados.core.logging import get_logger

from .errors import AirPipelineUnavailable

if TYPE_CHECKING:
    from ados.hal.camera import CameraInfo


def _pkg():
    """Return the air_pipeline package so test patches resolve at call time."""
    return sys.modules["ados.services.video.air_pipeline"]


def _available(name: str) -> bool:
    """Indirect through the barrel so test ``monkeypatch.setattr`` works."""
    return _pkg()._gst_element_available(name)


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
        if _available("libcamerasrc"):
            return (
                "libcamerasrc "
                f"! video/x-raw,width={width_int},height={height_int},"
                f"framerate={fps_int}/1,format=NV12 ",
                "libcamerasrc",
            )
        if soc_lower.startswith("bcm271") and _available("rpicamsrc"):
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
        # v1 — software encode from MJPEG-decoded raw is a future
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
        if soc_lower.startswith("bcm271") and _available("v4l2h264enc"):
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
        if soc_lower.startswith("rk") and _available("mpph264enc"):
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
            if _available("nvv4l2h264enc"):
                return (
                    f"nvv4l2h264enc bitrate={bitrate * 1000} "
                    f"iframeinterval={gop} insert-sps-pps=true "
                    "control-rate=1 "
                    "! video/x-h264,stream-format=byte-stream,alignment=au ",
                    "nvv4l2h264enc",
                    True,
                )
            if _available("omxh264enc"):
                return (
                    f"omxh264enc bitrate={bitrate * 1000} "
                    f"iframeinterval={gop} insert-sps-pps=true "
                    "control-rate=2 "
                    "! video/x-h264,stream-format=byte-stream,alignment=au ",
                    "omxh264enc",
                    True,
                )

    # Software fallback. Always available where libx264 is installed.
    if not _available("x264enc"):
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
