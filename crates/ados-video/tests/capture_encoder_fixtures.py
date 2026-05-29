#!/usr/bin/env python3
"""Capture exact argv vectors from the Python encoder command builder.

Run with the repo venv from the repo root:

    .venv/bin/python crates/ados-video/tests/capture_encoder_fixtures.py

It prints a JSON document of {case_name: argv_list} for a matrix of inputs.
Those captured vectors are pasted verbatim into encoder.rs as byte-parity
test fixtures.

All runtime probes that the Python builder performs (the Rockchip /proc
read, the ffmpeg -encoders probe, the gst-inspect probes, sys.executable)
are mocked here so every captured vector is fully deterministic and
reproducible on any host.
"""

from __future__ import annotations

import json
import logging
import sys
from dataclasses import dataclass

# Silence the structlog/stdlib loggers BEFORE importing the encoder so the only
# thing that lands on stdout is the JSON document. The encoder logs encoder
# selection at INFO and the SEI-skip at WARNING; route everything to stderr at
# CRITICAL so the captured stdout is pure JSON.
logging.disable(logging.CRITICAL)

# Import the real builder under test.
from ados.services.video import encoder as enc  # noqa: E402
from ados.hal.camera import CameraInfo, CameraType  # noqa: E402


class _NullLog:
    """No-op stand-in for the structlog logger so stdout stays pure JSON.

    The encoder logs encoder selection / SEI-skip events through a
    module-level structlog logger whose default factory writes to stdout.
    Replacing it with this swallow-everything object keeps the captured
    document free of log noise without changing any builder logic.
    """

    def __getattr__(self, _name):
        return lambda *a, **k: None


enc.log = _NullLog()  # type: ignore[assignment]


# --- deterministic value for sys.executable -------------------------------
# wrap_with_sei_inject reads sys.executable to splice the injector. On the rig
# this is the installed interpreter; pin it to a stable string so the captured
# fixture is host-independent. The Rust builder takes this as an explicit input.
PY_EXE = "/opt/ados/venv/bin/python3"


def mk_camera(cam_type: CameraType, caps: list[str], dev: str) -> CameraInfo:
    return CameraInfo(
        name="capture-camera",
        type=cam_type,
        device_path=dev,
        width=0,
        height=0,
        capabilities=list(caps),
    )


@dataclass
class Env:
    """Mockable runtime environment for the encoder builder."""

    is_rockchip: bool
    hw_h264: str | None  # ffmpeg HW H.264 encoder name, or None
    has_mpph264enc: bool = False
    has_rtspclientsink: bool = True


def install_env_mocks(env: Env) -> None:
    """Monkeypatch the runtime probes inside the encoder module."""
    # Rockchip /proc short-circuit + GStreamer _is_rockchip.
    enc._is_rockchip = lambda: env.is_rockchip  # type: ignore[assignment]

    # _detect_hw_h264_encoder: on Rockchip the Python code short-circuits to
    # None BEFORE the ffmpeg probe; mirror that exactly here so we capture the
    # same branch the rig takes.
    def fake_hw_h264() -> str | None:
        if env.is_rockchip:
            return None
        return env.hw_h264

    enc._detect_hw_h264_encoder = fake_hw_h264  # type: ignore[assignment]

    enc._has_mpph264enc = lambda: env.has_mpph264enc  # type: ignore[assignment]

    # The GStreamer RTSP path probes `gst-inspect-1.0 rtspclientsink` via
    # shutil.which + subprocess.run. Mock both shutil.which and subprocess.run
    # *inside the encoder module* so the rtspclientsink branch is deterministic.
    import shutil
    import subprocess

    orig_which = shutil.which

    def fake_which(name: str, *a, **k):
        if name == "gst-inspect-1.0":
            return "/usr/bin/gst-inspect-1.0"
        return orig_which(name, *a, **k)

    class _Probe:
        def __init__(self, rc: int) -> None:
            self.returncode = rc

    orig_run = subprocess.run

    def fake_run(cmd, *a, **k):
        if isinstance(cmd, (list, tuple)) and len(cmd) >= 2 and cmd[0] == "gst-inspect-1.0" and cmd[1] == "rtspclientsink":
            return _Probe(0 if env.has_rtspclientsink else 1)
        return orig_run(cmd, *a, **k)

    enc.shutil.which = fake_which  # type: ignore[attr-defined]
    enc.subprocess.run = fake_run  # type: ignore[attr-defined]


def build(case: dict) -> list[str]:
    """Build one argv from a case spec, applying its env + sei wrap."""
    install_env_mocks(case["env"])
    sys.executable = PY_EXE  # pin for wrap_with_sei_inject

    cfg = enc.EncoderConfig(
        type=case["enc_type"],
        codec=case.get("codec", "h264"),
        width=case["width"],
        height=case["height"],
        fps=case["fps"],
        bitrate_kbps=case["bitrate_kbps"],
    )
    cmd = enc.build_encoder_command(
        cfg, case["source"], case["output"], camera=case["camera"]
    )
    if case.get("sei"):
        cmd = enc.wrap_with_sei_inject(cmd, case["output"])
    return cmd


CSI = mk_camera(CameraType.CSI, ["h264", "mjpeg"], "/dev/video0")
USB_MJPEG = mk_camera(CameraType.USB, ["mjpeg", "yuyv"], "/dev/video1")
USB_YUYV = mk_camera(CameraType.USB, ["yuyv"], "/dev/video2")
IP_CAM = mk_camera(CameraType.IP, ["rtsp"], "rtsp://10.0.0.9:554/live")

ROCKCHIP = Env(is_rockchip=True, hw_h264=None)
NON_RK_SW = Env(is_rockchip=False, hw_h264=None)  # no HW ffmpeg encoder
NON_RK_HW = Env(is_rockchip=False, hw_h264="h264_v4l2m2m")  # Pi HW encoder
RK_MPP = Env(is_rockchip=True, hw_h264=None, has_mpph264enc=True)

RTSP_OUT = "rtsp://127.0.0.1:8554/main"
UDP_OUT = "udp://127.0.0.1:5600"

E = enc.EncoderType

CASES: dict[str, dict] = {
    # --- CSI → rpicam (RTSP bash pipeline) -------------------------------
    "csi_rpicam_rtsp_rk": {
        "enc_type": E.RPICAM_VID, "camera": CSI, "source": "/dev/video0",
        "output": RTSP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": ROCKCHIP,
    },
    "csi_rpicam_rtsp_rk_sei": {
        "enc_type": E.RPICAM_VID, "camera": CSI, "source": "/dev/video0",
        "output": RTSP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": ROCKCHIP, "sei": True,
    },
    "csi_rpicam_file": {
        "enc_type": E.RPICAM_VID, "camera": CSI, "source": "/dev/video0",
        "output": "/var/lib/ados/out.h264", "width": 1920, "height": 1080,
        "fps": 60, "bitrate_kbps": 8000, "env": ROCKCHIP,
    },

    # --- USB MJPEG → ffmpeg libx264 (Rockchip: HW short-circuit → SW) ----
    "usb_mjpeg_ffmpeg_rtsp_rk": {
        "enc_type": E.FFMPEG, "camera": USB_MJPEG, "source": "/dev/video1",
        "output": RTSP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": ROCKCHIP,
    },
    "usb_mjpeg_ffmpeg_rtsp_rk_sei": {
        "enc_type": E.FFMPEG, "camera": USB_MJPEG, "source": "/dev/video1",
        "output": RTSP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": ROCKCHIP, "sei": True,
    },
    "usb_mjpeg_ffmpeg_rtsp_rk_640x480_15": {
        "enc_type": E.FFMPEG, "camera": USB_MJPEG, "source": "/dev/video1",
        "output": RTSP_OUT, "width": 640, "height": 480, "fps": 15,
        "bitrate_kbps": 1500, "env": ROCKCHIP,
    },
    "usb_mjpeg_ffmpeg_udp_rk": {
        "enc_type": E.FFMPEG, "camera": USB_MJPEG, "source": "/dev/video1",
        "output": UDP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": ROCKCHIP,
    },
    "usb_mjpeg_ffmpeg_udp_rk_sei": {
        "enc_type": E.FFMPEG, "camera": USB_MJPEG, "source": "/dev/video1",
        "output": UDP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": ROCKCHIP, "sei": True,
    },

    # --- USB YUYV → ffmpeg libx264 --------------------------------------
    "usb_yuyv_ffmpeg_rtsp_rk": {
        "enc_type": E.FFMPEG, "camera": USB_YUYV, "source": "/dev/video2",
        "output": RTSP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": ROCKCHIP,
    },

    # --- USB on NON-Rockchip with HW ffmpeg encoder (h264_v4l2m2m) ------
    "usb_mjpeg_ffmpeg_rtsp_pi_hw": {
        "enc_type": E.FFMPEG, "camera": USB_MJPEG, "source": "/dev/video1",
        "output": RTSP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": NON_RK_HW,
    },
    "usb_mjpeg_ffmpeg_rtsp_pi_hw_sei": {
        "enc_type": E.FFMPEG, "camera": USB_MJPEG, "source": "/dev/video1",
        "output": RTSP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": NON_RK_HW, "sei": True,
    },

    # --- USB on NON-Rockchip WITHOUT HW encoder (libx264 software) ------
    "usb_mjpeg_ffmpeg_rtsp_nonrk_sw": {
        "enc_type": E.FFMPEG, "camera": USB_MJPEG, "source": "/dev/video1",
        "output": RTSP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": NON_RK_SW,
    },

    # --- IP camera → ffmpeg (network input, no v4l2 wrapper) ------------
    "ip_ffmpeg_rtsp_rk": {
        "enc_type": E.FFMPEG, "camera": IP_CAM,
        "source": "rtsp://10.0.0.9:554/live", "output": RTSP_OUT,
        "width": 1280, "height": 720, "fps": 30, "bitrate_kbps": 4000,
        "env": ROCKCHIP,
    },

    # --- GStreamer path: Rockchip mpph264enc + rtspclientsink -----------
    "gst_usb_mjpeg_rtsp_rk_mpp": {
        "enc_type": E.GSTREAMER, "camera": USB_MJPEG, "source": "/dev/video1",
        "output": RTSP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": RK_MPP,
    },
    # GStreamer path: Rockchip mpph264enc, NO rtspclientsink → ffmpeg pipe
    "gst_usb_mjpeg_rtsp_rk_mpp_noclient": {
        "enc_type": E.GSTREAMER, "camera": USB_MJPEG, "source": "/dev/video1",
        "output": RTSP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000,
        "env": Env(is_rockchip=True, hw_h264=None, has_mpph264enc=True,
                   has_rtspclientsink=False),
    },
    # GStreamer software x264enc fallback (non-Rockchip), rtspclientsink
    "gst_usb_yuyv_rtsp_nonrk_x264": {
        "enc_type": E.GSTREAMER, "camera": USB_YUYV, "source": "/dev/video2",
        "output": RTSP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": NON_RK_SW,
    },
    # GStreamer file output (direct pipeline, no rtsp)
    "gst_usb_mjpeg_file_rk_mpp": {
        "enc_type": E.GSTREAMER, "camera": USB_MJPEG, "source": "/dev/video1",
        "output": "/var/lib/ados/cap.h264", "width": 1280, "height": 720,
        "fps": 30, "bitrate_kbps": 4000, "env": RK_MPP,
    },
    # GStreamer + SEI wrap (case 3: skipped, returned unchanged)
    "gst_usb_mjpeg_rtsp_rk_mpp_sei_skip": {
        "enc_type": E.GSTREAMER, "camera": USB_MJPEG, "source": "/dev/video1",
        "output": RTSP_OUT, "width": 1280, "height": 720, "fps": 30,
        "bitrate_kbps": 4000, "env": RK_MPP, "sei": True,
    },
}


def main() -> None:
    out: dict[str, list[str]] = {}
    for name, spec in CASES.items():
        out[name] = build(spec)
    json.dump(out, sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
