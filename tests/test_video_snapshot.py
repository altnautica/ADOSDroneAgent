"""Tests for the video snapshot capture."""

from __future__ import annotations

import tempfile
from unittest.mock import AsyncMock, patch

import pytest

from ados.hal.camera import CameraInfo, CameraType
from ados.services.video.snapshot import (
    _build_capture_command,
    _write_exif,
    capture_snapshot,
)


class TestBuildCaptureCommand:
    def test_csi_with_rpicam(self):
        cam = CameraInfo(name="CSI-0", type=CameraType.CSI, device_path="/dev/video0")
        mock_target = "ados.services.video.snapshot.shutil.which"
        with patch(mock_target, return_value="/usr/bin/rpicam-still"):
            cmd = _build_capture_command(cam, "/tmp/snap.jpg")
            assert cmd[0] == "rpicam-still"
            assert "/tmp/snap.jpg" in cmd

    def test_csi_without_rpicam_uses_ffmpeg(self):
        cam = CameraInfo(name="CSI-0", type=CameraType.CSI, device_path="/dev/video0")
        with patch("ados.services.video.snapshot.shutil.which", return_value=None):
            cmd = _build_capture_command(cam, "/tmp/snap.jpg")
            assert cmd[0] == "ffmpeg"

    def test_usb_camera(self):
        cam = CameraInfo(name="USB", type=CameraType.USB, device_path="/dev/video2")
        cmd = _build_capture_command(cam, "/tmp/snap.jpg")
        assert cmd[0] == "ffmpeg"
        assert "-f" in cmd
        assert "v4l2" in cmd

    def test_ip_camera(self):
        cam = CameraInfo(
            name="IP-0",
            type=CameraType.IP,
            device_path="rtsp://10.0.0.1/stream",
        )
        cmd = _build_capture_command(cam, "/tmp/snap.jpg")
        assert cmd[0] == "ffmpeg"
        assert "-rtsp_transport" in cmd


class TestWriteExif:
    def test_no_piexif_returns_false(self):
        with patch.dict("sys.modules", {"piexif": None}):
            # When piexif can't be imported, should return False gracefully
            result = _write_exif("/tmp/test.jpg", 12.97, 77.59)
            assert result is False


@pytest.mark.asyncio
class TestCaptureSnapshot:
    async def test_capture_tool_not_found(self):
        cam = CameraInfo(name="Test", type=CameraType.USB, device_path="/dev/video0")

        with patch(
            "ados.services.video.snapshot.asyncio.create_subprocess_exec",
            side_effect=FileNotFoundError,
        ):
            with tempfile.TemporaryDirectory() as tmpdir:
                result = await capture_snapshot(cam, tmpdir)
                assert result == ""

    async def test_capture_nonzero_exit(self):
        cam = CameraInfo(name="Test", type=CameraType.USB, device_path="/dev/video0")

        mock_proc = AsyncMock()
        mock_proc.returncode = 1
        mock_proc.communicate = AsyncMock(return_value=(b"", b"error msg"))

        with patch(
            "ados.services.video.snapshot.asyncio.create_subprocess_exec",
            return_value=mock_proc,
        ):
            with tempfile.TemporaryDirectory() as tmpdir:
                result = await capture_snapshot(cam, tmpdir)
                assert result == ""

    async def test_capture_success(self):
        cam = CameraInfo(name="Test", type=CameraType.USB, device_path="/dev/video0")

        mock_proc = AsyncMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))

        with patch(
            "ados.services.video.snapshot.asyncio.create_subprocess_exec",
            return_value=mock_proc,
        ):
            with tempfile.TemporaryDirectory() as tmpdir:
                result = await capture_snapshot(cam, tmpdir)
                assert result != ""
                assert result.endswith(".jpg")

    async def test_creates_output_dir(self):
        cam = CameraInfo(name="Test", type=CameraType.USB, device_path="/dev/video0")

        mock_proc = AsyncMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))

        with (
            patch(
                "ados.services.video.snapshot.asyncio.create_subprocess_exec",
                return_value=mock_proc,
            ),
            tempfile.TemporaryDirectory() as tmpdir,
        ):
            new_dir = f"{tmpdir}/snapshots/new"
            result = await capture_snapshot(cam, new_dir)
            assert result != ""
