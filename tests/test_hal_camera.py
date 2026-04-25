"""Tests for HAL camera discovery."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

from ados.hal.camera import (
    CameraInfo,
    CameraType,
    _cameras_from_config,
    _discover_csi_cameras,
    _discover_usb_cameras,
    discover_cameras,
)


class TestCameraInfo:
    def test_defaults(self):
        cam = CameraInfo(name="Test", type=CameraType.USB, device_path="/dev/video0")
        assert cam.width == 0
        assert cam.height == 0
        assert cam.capabilities == []

    def test_to_dict(self):
        cam = CameraInfo(
            name="CSI-0",
            type=CameraType.CSI,
            device_path="/dev/video0",
            width=1920,
            height=1080,
            capabilities=["h264"],
        )
        d = cam.to_dict()
        assert d["name"] == "CSI-0"
        assert d["type"] == "csi"
        assert d["width"] == 1920


class TestCameraTypeEnum:
    def test_values(self):
        assert CameraType.CSI == "csi"
        assert CameraType.USB == "usb"
        assert CameraType.IP == "ip"


class TestDiscoverCsiCameras:
    def test_rpicam_not_found(self):
        with patch("ados.hal.camera.subprocess.run", side_effect=FileNotFoundError):
            result = _discover_csi_cameras()
            assert result == []

    def test_rpicam_success(self):
        output = "0 : imx219 [3280x2464 10-bit RGGB] (/base/soc/i2c/imx219)\n"
        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_result.stderr = output
        mock_result.stdout = ""

        with patch("ados.hal.camera.subprocess.run", return_value=mock_result):
            cameras = _discover_csi_cameras()
            assert len(cameras) == 1
            assert cameras[0].type == CameraType.CSI
            assert cameras[0].width == 3280
            assert cameras[0].height == 2464

    def test_rpicam_failure_returncode(self):
        mock_result = MagicMock()
        mock_result.returncode = 1
        with patch("ados.hal.camera.subprocess.run", return_value=mock_result):
            assert _discover_csi_cameras() == []


class TestDiscoverUsbCameras:
    def test_v4l2_not_found(self):
        with patch("ados.hal.camera.subprocess.run", side_effect=FileNotFoundError):
            result = _discover_usb_cameras()
            assert result == []

    def test_v4l2_success(self):
        output = (
            "HD Pro Webcam C920 (usb-0000:00:14.0-1):\n"
            "\t/dev/video0\n"
            "\t/dev/video1\n"
            "\n"
        )
        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_result.stdout = output

        with patch("ados.hal.camera.subprocess.run", return_value=mock_result):
            cameras = _discover_usb_cameras()
            # Multiple /dev/videoN entries belonging to the same physical USB
            # device collapse into one CameraInfo (capture node only).
            assert len(cameras) >= 1
            assert cameras[0].type == CameraType.USB
            assert cameras[0].name == "HD Pro Webcam C920"
            assert cameras[0].device_path == "/dev/video0"

    def test_v4l2_failure_returncode(self):
        mock_result = MagicMock()
        mock_result.returncode = 1
        with patch("ados.hal.camera.subprocess.run", return_value=mock_result):
            assert _discover_usb_cameras() == []


class TestCamerasFromConfig:
    def test_empty_list(self):
        assert _cameras_from_config([]) == []

    def test_ip_cameras(self):
        sources = [
            {"url": "rtsp://192.168.1.100:554/stream1", "name": "Roof Camera"},
            {"url": "rtsp://192.168.1.101:554/stream1"},
        ]
        cameras = _cameras_from_config(sources)
        assert len(cameras) == 2
        assert cameras[0].name == "Roof Camera"
        assert cameras[0].type == CameraType.IP
        assert cameras[1].name == "IP-1"

    def test_empty_url_skipped(self):
        sources = [{"url": ""}, {"name": "no-url"}]
        cameras = _cameras_from_config(sources)
        assert len(cameras) == 0


class TestDiscoverCameras:
    def test_macos_returns_only_ip(self):
        ip = [{"url": "rtsp://10.0.0.1/stream"}]
        with patch("ados.hal.camera.platform.system", return_value="Darwin"):
            cameras = discover_cameras(ip_sources=ip)
            # On macOS only IP cameras are returned
            assert len(cameras) == 1
            assert cameras[0].type == CameraType.IP

    def test_linux_runs_discovery(self):
        with (
            patch("ados.hal.camera.platform.system", return_value="Linux"),
            patch("ados.hal.camera._discover_csi_cameras", return_value=[]),
            patch("ados.hal.camera._discover_usb_cameras", return_value=[]),
        ):
            cameras = discover_cameras()
            assert cameras == []

    def test_no_ip_sources(self):
        with patch("ados.hal.camera.platform.system", return_value="Darwin"):
            cameras = discover_cameras()
            assert cameras == []
