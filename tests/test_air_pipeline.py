"""Pipeline string + tier-selection tests for the air-side GStreamer chain.

PyGObject is not required for these tests — we only exercise the pure
Python helpers that compose the gst-launch string. The element-existence
probe is monkeypatched so the same test runs identically on CI hosts
without GStreamer installed and on a real Pi 4B with v4l2h264enc
present.
"""

from __future__ import annotations

from dataclasses import dataclass

import pytest

from ados.services.video import air_pipeline as ap


@dataclass
class _StubCameraType:
    value: str


@dataclass
class _StubCamera:
    type: _StubCameraType
    device_path: str = ""
    name: str = "stub-cam"


def _usb_cam(path: str = "/dev/video0") -> _StubCamera:
    return _StubCamera(type=_StubCameraType("usb"), device_path=path)


def _csi_cam() -> _StubCamera:
    return _StubCamera(type=_StubCameraType("csi"), device_path="/dev/video0")


def _patch_gst_available(monkeypatch: pytest.MonkeyPatch, present: set[str]) -> None:
    """Force a specific set of GStreamer factory names to look available."""
    monkeypatch.setattr(
        ap, "_gst_element_available", lambda name: name in present
    )


# ── camera source dispatch ───────────────────────────────────────


def test_camera_source_videotestsrc_when_camera_missing(monkeypatch):
    _patch_gst_available(monkeypatch, set())
    element_str, kind = ap.choose_camera_source(
        None, soc="BCM2711", width=640, height=480, fps=30
    )
    assert kind == "videotestsrc"
    assert "videotestsrc" in element_str
    assert "width=640" in element_str and "framerate=30/1" in element_str


def test_camera_source_v4l2src_for_usb_camera(monkeypatch):
    _patch_gst_available(monkeypatch, set())
    element_str, kind = ap.choose_camera_source(
        _usb_cam("/dev/video2"), soc="BCM2711", width=1280, height=720, fps=30
    )
    assert kind == "v4l2src"
    assert "v4l2src device=/dev/video2" in element_str
    assert "videoconvert" in element_str  # YUY2 -> I420 conversion


def test_camera_source_libcamerasrc_when_available(monkeypatch):
    _patch_gst_available(monkeypatch, {"libcamerasrc"})
    element_str, kind = ap.choose_camera_source(
        _csi_cam(), soc="BCM2711", width=1280, height=720, fps=30
    )
    assert kind == "libcamerasrc"
    assert element_str.startswith("libcamerasrc")


def test_camera_source_rpicamsrc_for_pi_legacy(monkeypatch):
    # No libcamerasrc, falls through to rpicamsrc on a BCM SoC.
    _patch_gst_available(monkeypatch, {"rpicamsrc"})
    element_str, kind = ap.choose_camera_source(
        _csi_cam(), soc="BCM2711", width=1280, height=720, fps=30
    )
    assert kind == "rpicamsrc"
    assert "rpicamsrc" in element_str


# ── encoder tier ────────────────────────────────────────────────


def test_encoder_v4l2h264enc_preferred_on_pi(monkeypatch):
    _patch_gst_available(monkeypatch, {"v4l2h264enc", "x264enc"})
    element_str, name, hw = ap.choose_encoder(
        soc="BCM2711",
        hw_video_codecs=["h264_enc", "h264_dec"],
        bitrate_kbps=4000,
        keyframe_interval=30,
        prefer_hw=True,
    )
    assert name == "v4l2h264enc"
    assert hw is True
    assert "video_bitrate=4000000" in element_str
    assert "alignment=au" in element_str


def test_encoder_mpph264enc_preferred_on_rockchip(monkeypatch):
    _patch_gst_available(monkeypatch, {"mpph264enc", "x264enc"})
    element_str, name, hw = ap.choose_encoder(
        soc="RK3582",
        hw_video_codecs=["h264_enc", "h264_dec"],
        bitrate_kbps=4000,
        keyframe_interval=30,
        prefer_hw=True,
    )
    assert name == "mpph264enc"
    assert hw is True
    assert "bps=4000000" in element_str


def test_encoder_falls_back_to_x264enc_when_hw_unavailable(monkeypatch):
    _patch_gst_available(monkeypatch, {"x264enc"})
    element_str, name, hw = ap.choose_encoder(
        soc="BCM2711",
        hw_video_codecs=["h264_enc"],
        bitrate_kbps=4000,
        keyframe_interval=30,
        prefer_hw=True,
    )
    assert name == "x264enc"
    assert hw is False
    assert "tune=zerolatency" in element_str
    assert "bitrate=4000" in element_str  # x264enc takes kbps directly


def test_encoder_force_software_with_prefer_hw_false(monkeypatch):
    _patch_gst_available(
        monkeypatch, {"v4l2h264enc", "mpph264enc", "x264enc"}
    )
    _, name, hw = ap.choose_encoder(
        soc="BCM2711",
        hw_video_codecs=["h264_enc"],
        bitrate_kbps=4000,
        keyframe_interval=30,
        prefer_hw=False,
    )
    assert name == "x264enc"
    assert hw is False


def test_encoder_skipped_when_board_does_not_advertise_h264_enc(monkeypatch):
    # Even when v4l2h264enc element exists, a board profile without
    # h264_enc capability means the kernel driver / DTS is not wired:
    # we must fall back to software.
    _patch_gst_available(monkeypatch, {"v4l2h264enc", "x264enc"})
    _, name, hw = ap.choose_encoder(
        soc="BCM2711",
        hw_video_codecs=["h264_dec"],
        bitrate_kbps=4000,
        keyframe_interval=30,
        prefer_hw=True,
    )
    assert name == "x264enc"
    assert hw is False


def test_encoder_raises_when_no_x264enc(monkeypatch):
    _patch_gst_available(monkeypatch, set())
    with pytest.raises(ap.AirPipelineUnavailable):
        ap.choose_encoder(
            soc="BCM2711",
            hw_video_codecs=[],
            bitrate_kbps=4000,
            keyframe_interval=30,
            prefer_hw=True,
        )


# ── full pipeline string composition ────────────────────────────


def test_pipeline_string_wfb_branch_and_cloud_gate_closed(monkeypatch):
    _patch_gst_available(monkeypatch, {"x264enc"})
    pipeline_str, meta = ap.build_air_pipeline_string(
        camera=_usb_cam(),
        soc="BCM2711",
        hw_video_codecs=["h264_enc"],
        width=1280,
        height=720,
        fps=30,
        bitrate_kbps=4000,
        keyframe_interval=30,
        cloud_branch_enabled=False,
        cloud_rtp_port=8000,
        prefer_hw_encoder=True,
    )
    # Both sinks wired.
    assert "udpsink name=wfb_sink host=127.0.0.1 port=5600" in pipeline_str
    assert "udpsink name=cloud_sink host=127.0.0.1 port=8000" in pipeline_str
    # Cloud gate dropping buffers by default when cloud relay off.
    assert "identity name=cloud_gate drop-buffers=true" in pipeline_str
    # SEI-relevant identity (named h264parse + named rtph264pay).
    assert "h264parse name=h264parse_air" in pipeline_str
    assert "rtph264pay name=rtph264pay_air" in pipeline_str
    # MTU + payload type + ssrc all pinned.
    assert "mtu=1316" in pipeline_str
    assert "pt=96" in pipeline_str
    assert "ssrc=51966" in pipeline_str  # 0xCAFE
    # Metadata reflects the chosen camera + encoder.
    assert meta["camera_source"] == "v4l2src"
    assert meta["encoder_name"] == "x264enc"
    assert meta["encoder_hw_accel"] is False
    assert meta["cloud_branch_enabled"] is False


def test_pipeline_string_cloud_gate_opens_when_enabled(monkeypatch):
    _patch_gst_available(monkeypatch, {"x264enc"})
    pipeline_str, meta = ap.build_air_pipeline_string(
        camera=_usb_cam(),
        soc="BCM2711",
        hw_video_codecs=["h264_enc"],
        width=1280,
        height=720,
        fps=30,
        bitrate_kbps=4000,
        keyframe_interval=30,
        cloud_branch_enabled=True,
        cloud_rtp_port=8000,
        prefer_hw_encoder=True,
    )
    assert "identity name=cloud_gate drop-buffers=false" in pipeline_str
    assert meta["cloud_branch_enabled"] is True
