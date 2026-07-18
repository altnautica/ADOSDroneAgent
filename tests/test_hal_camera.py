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

        with (
            patch("ados.hal.camera.subprocess.run", return_value=mock_result),
            patch("ados.hal.camera._video_node_openable", return_value=True),
        ):
            cameras = _discover_usb_cameras()
            # Multiple /dev/videoN entries belonging to the same physical USB
            # device collapse into one CameraInfo (capture node only).
            assert len(cameras) >= 1
            assert cameras[0].type == CameraType.USB
            assert cameras[0].name == "HD Pro Webcam C920"
            assert cameras[0].device_path == "/dev/video0"

    def test_v4l2_stale_node_dropped(self):
        # A node that v4l2-ctl still lists but which no longer opens (a
        # ghost from a recently unplugged camera) must not be returned —
        # otherwise the pipeline launches an encoder against a dead device.
        output = (
            "HD Pro Webcam C920 (usb-0000:00:14.0-1):\n"
            "\t/dev/video0\n"
            "\t/dev/video1\n"
            "\n"
        )
        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_result.stdout = output

        with (
            patch("ados.hal.camera.subprocess.run", return_value=mock_result),
            patch("ados.hal.camera._video_node_openable", return_value=False),
        ):
            assert _discover_usb_cameras() == []

    def test_v4l2_failure_returncode(self):
        mock_result = MagicMock()
        mock_result.returncode = 1
        with patch("ados.hal.camera.subprocess.run", return_value=mock_result):
            assert _discover_usb_cameras() == []

    def test_v4l2_pi_internal_devices_filtered(self):
        # Raspberry Pi exposes the bcm2835 codec, ISP, hevc decoder, and
        # unicam capture interface through /dev/videoN — none of these
        # are capturable cameras for our purposes. With a real USB camera
        # plugged in, only the camera should land in the result.
        output = (
            "bcm2835-codec-decode (platform:bcm2835-codec):\n"
            "\t/dev/video10\n"
            "\n"
            "bcm2835-isp (platform:bcm2835-isp):\n"
            "\t/dev/video13\n"
            "\n"
            "unicam (platform:fe801000.csi):\n"
            "\t/dev/video0\n"
            "\n"
            "rpi-hevc-dec (platform:rpi-hevc-dec):\n"
            "\t/dev/video19\n"
            "\n"
            "Logitech HD Pro Webcam C920 (usb-0000:00:14.0-1):\n"
            "\t/dev/video2\n"
            "\t/dev/video3\n"
            "\n"
        )
        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_result.stdout = output

        with (
            patch("ados.hal.camera.subprocess.run", return_value=mock_result),
            patch("ados.hal.camera._video_node_openable", return_value=True),
        ):
            cameras = _discover_usb_cameras()
            names = {c.name for c in cameras}
            assert names == {"Logitech HD Pro Webcam C920"}
            # No internal device should appear in the camera list.
            for forbidden in ("bcm2835", "unicam", "hevc", "isp", "codec"):
                for c in cameras:
                    assert forbidden not in c.name.lower()


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


class TestFingerprints:
    def test_to_dict_carries_match(self):
        cam = CameraInfo(
            name="belly",
            type=CameraType.USB,
            device_path="/dev/video0",
            match={"usb": "046d:0825"},
        )
        assert cam.to_dict()["match"] == {"usb": "046d:0825"}

    def test_csi_match_has_sensor_and_port(self):
        output = "1 : imx219 [3280x2464 10-bit RGGB] (/base/soc/i2c/imx219)\n"
        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_result.stderr = output
        mock_result.stdout = ""
        with patch("ados.hal.camera.subprocess.run", return_value=mock_result):
            cameras = _discover_csi_cameras()
            assert cameras[0].match == {"csi_sensor": "imx219", "csi_port": 1}

    def test_usb_match_is_empty_without_sysfs(self):
        from ados.hal.camera import _usb_match

        # A node with no sysfs backing (a non-USB device, a non-Linux host)
        # degrades to an empty fingerprint rather than raising.
        assert _usb_match("/dev/video99") == {}

    def test_usb_match_reads_vid_pid_serial(self, tmp_path, monkeypatch):
        from ados.hal import camera as camera_mod

        # Build a fake sysfs tree: the video node's `device` link points at a UVC
        # interface dir whose parent USB device dir carries idVendor/idProduct/serial.
        usb_dev = tmp_path / "1-1"
        iface = usb_dev / "1-1:1.0"
        iface.mkdir(parents=True)
        (usb_dev / "idVendor").write_text("046D\n")
        (usb_dev / "idProduct").write_text("0825\n")
        (usb_dev / "serial").write_text("ABC123\n")
        monkeypatch.setattr(
            camera_mod.os.path,
            "realpath",
            lambda p: str(iface),
        )
        assert camera_mod._usb_match("/dev/video7") == {"usb": "046d:0825:ABC123"}

    def test_write_discovery_sidecar_round_trips(self, tmp_path):
        import json

        from ados.hal.camera import write_discovery_sidecar

        path = tmp_path / "cameras-discovered.json"
        cams = [
            CameraInfo(
                name="CSI-0 (imx219)",
                type=CameraType.CSI,
                device_path="/dev/video0",
                width=3280,
                height=2464,
                capabilities=["h264", "mjpeg"],
                match={"csi_sensor": "imx219", "csi_port": 0},
            ),
        ]
        assert write_discovery_sidecar(cams, path=str(path)) is True
        blob = json.loads(path.read_text())
        assert blob["version"] == 1
        assert blob["updated_at_unix"] > 0
        assert len(blob["cameras"]) == 1
        assert blob["cameras"][0]["device_path"] == "/dev/video0"
        assert blob["cameras"][0]["match"] == {"csi_sensor": "imx219", "csi_port": 0}

    def test_write_discovery_sidecar_survives_a_bad_path(self):
        from ados.hal.camera import write_discovery_sidecar

        # A write into a path that cannot be created returns False, never raises.
        assert write_discovery_sidecar([], path="/nonexistent-root/deny/x.json") is False
