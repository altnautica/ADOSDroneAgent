"""V4L2 camera discovery reads what each node offers and classifies it by
its kernel driver. Recorded ``v4l2-ctl`` output lives in tests/fixtures/v4l2."""

from __future__ import annotations

import json
import os
from pathlib import Path
from unittest.mock import MagicMock, patch

from ados.hal.camera import CameraType, _discover_usb_cameras
from ados.hal.v4l2_formats import (
    best_mode,
    capabilities,
    parse_list_formats_ext,
    v4l2_driver,
)

FIXTURES = Path(__file__).parent / "fixtures" / "v4l2"


def _fixture(name: str) -> str:
    return (FIXTURES / name).read_text()


class TestParseListFormatsExt:
    def test_uvc_mjpeg_and_yuyv(self):
        formats = parse_list_formats_ext(_fixture("formats-uvc-mjpeg-yuyv.txt"))
        assert capabilities(formats) == ["mjpeg", "yuyv"]
        assert formats[0] == {
            "fourcc": "mjpeg",
            "width": 1920,
            "height": 1080,
            "fps": [30.0, 25.0, 15.0],
        }
        assert formats[1]["fps"] == [60.0, 30.0]
        assert [(f["fourcc"], f["width"], f["height"]) for f in formats[3:]] == [
            ("yuyv", 640, 480),
            ("yuyv", 1280, 720),
            ("yuyv", 1920, 1080),
        ]
        # Largest size, then its fastest rate.
        assert best_mode(formats) == (1920, 1080, 30.0)

    def test_yuyv_only(self):
        formats = parse_list_formats_ext(_fixture("formats-uvc-yuyv-only.txt"))
        assert capabilities(formats) == ["yuyv"]
        assert formats[1] == {"fourcc": "yuyv", "width": 320, "height": 240, "fps": [30.0, 20.0]}
        assert best_mode(formats) == (640, 480, 30.0)

    def test_stepwise_continuous_and_sizeless_formats(self):
        formats = parse_list_formats_ext(_fixture("formats-csi-mplane-stepwise.txt"))
        assert formats == [
            {"fourcc": "nv12", "width": 3840, "height": 2160, "fps": [30.0]},
            {"fourcc": "nv21", "width": 0, "height": 0, "fps": []},
            {"fourcc": "yu12", "width": 1920, "height": 1080, "fps": [60.0]},
        ]
        assert best_mode(formats) == (3840, 2160, 30.0)

    def test_nothing_offered(self):
        formats = parse_list_formats_ext(_fixture("formats-csi-no-sensor.txt"))
        assert formats == []
        assert capabilities(formats) == []
        assert best_mode(formats) == (0, 0, 0.0)


class TestV4l2Driver:
    def test_reads_the_bound_driver_from_sysfs(self, tmp_path):
        dev = tmp_path / "devices" / "usb1" / "1-1" / "1-1:1.0"
        dev.mkdir(parents=True)
        drivers = tmp_path / "bus" / "usb" / "drivers" / "uvcvideo"
        drivers.mkdir(parents=True)
        os.symlink(drivers, dev / "driver")
        node = tmp_path / "class" / "video4linux" / "video2"
        node.mkdir(parents=True)
        os.symlink(dev, node / "device")
        assert v4l2_driver("/dev/video2", sys_root=str(tmp_path)) == "uvcvideo"
        assert v4l2_driver("/dev/video7", sys_root=str(tmp_path)) is None


def _discover(drivers: dict[str, str | None], formats: dict[str, str]):
    """Run discovery over the recorded --list-devices output, each node's
    driver and recorded --list-formats-ext output given per node."""

    def fake_run(argv, **_kw):
        result = MagicMock()
        result.returncode = 0
        if argv[1] == "--list-devices":
            result.stdout = _fixture("list-devices-csi-and-uvc.txt")
        else:
            name = formats.get(argv[-1])
            result.stdout = _fixture(name) if name else ""
        return result

    with (
        patch("ados.hal.camera.subprocess.run", side_effect=fake_run),
        patch("ados.hal.camera.v4l2_driver", side_effect=lambda p: drivers.get(p)),
        patch("ados.hal.camera._video_node_openable", return_value=True),
        patch("ados.hal.camera._usb_match", return_value={}),
    ):
        return _discover_usb_cameras()


class TestDiscoveryByDriver:
    DRIVERS = {
        "/dev/video0": "sunxi-vin",
        "/dev/video1": "sunxi-vin",
        "/dev/video2": "uvcvideo",
        "/dev/video4": "uvcvideo",
    }

    def test_types_capabilities_and_sizes_come_from_the_node(self):
        cameras = _discover(
            self.DRIVERS,
            {
                "/dev/video0": "formats-csi-mplane-stepwise.txt",
                "/dev/video2": "formats-uvc-mjpeg-yuyv.txt",
                "/dev/video4": "formats-uvc-yuyv-only.txt",
            },
        )
        got = [
            (c.device_path, c.type, c.capabilities, c.width, c.height, c.fps)
            for c in cameras
        ]
        assert got == [
            ("/dev/video0", CameraType.CSI, ["nv12", "nv21", "yu12"], 3840, 2160, 30.0),
            ("/dev/video2", CameraType.USB, ["mjpeg", "yuyv"], 1920, 1080, 30.0),
            ("/dev/video4", CameraType.USB, ["yuyv"], 640, 480, 30.0),
        ]
        d = cameras[2].to_dict()
        assert d["fps"] == 30.0
        assert d["formats"][0] == {"fourcc": "yuyv", "width": 640, "height": 480, "fps": [30.0]}
        # The orchestrator reads this JSON; it stays plain data.
        assert json.loads(json.dumps(d)) == d

    def test_a_csi_node_with_no_sensor_is_not_a_camera(self):
        cameras = _discover(
            self.DRIVERS,
            {
                "/dev/video0": "formats-csi-no-sensor.txt",
                "/dev/video1": "formats-csi-no-sensor.txt",
                "/dev/video2": "formats-uvc-mjpeg-yuyv.txt",
                "/dev/video4": "formats-uvc-yuyv-only.txt",
            },
        )
        assert [c.device_path for c in cameras] == ["/dev/video2", "/dev/video4"]
        assert all(c.type == CameraType.USB for c in cameras)

    def test_a_uvc_camera_whose_formats_cannot_be_read_claims_none(self):
        cameras = _discover({"/dev/video2": "uvcvideo"}, {})
        uvc = next(c for c in cameras if c.device_path == "/dev/video2")
        assert (uvc.capabilities, uvc.formats, uvc.width, uvc.height) == ([], [], 0, 0)

    def test_an_unknown_driver_keeps_the_name_filter(self):
        # No driver readable: the CSI block falls back to the name filter,
        # which does not match "sunxi-vin", so it is listed as before (USB).
        cameras = _discover({}, {"/dev/video0": "formats-uvc-yuyv-only.txt"})
        assert cameras[0].device_path == "/dev/video0"
        assert cameras[0].type == CameraType.USB
