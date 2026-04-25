"""Tests for the video pipeline service."""

from __future__ import annotations

import os
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.core.config import VideoConfig
from ados.services.video.pipeline import PipelineState, VideoPipeline


class TestPipelineState:
    def test_enum_values(self):
        assert PipelineState.STOPPED == "stopped"
        assert PipelineState.STARTING == "starting"
        assert PipelineState.RUNNING == "running"
        assert PipelineState.ERROR == "error"


class TestVideoPipeline:
    def test_initial_state(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        assert pipeline.state == PipelineState.STOPPED
        assert pipeline.camera_manager is not None
        assert pipeline.recorder is not None
        assert pipeline.mediamtx is not None

    def test_get_status_when_stopped(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        status = pipeline.get_status()
        assert status["state"] == "stopped"
        assert status["encoder"] is None
        assert "cameras" in status
        assert "recorder" in status
        assert "mediamtx" in status

    @pytest.mark.asyncio
    async def test_start_stream_no_cameras(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)

        with patch("ados.services.video.pipeline.discover_cameras", return_value=[]):
            result = await pipeline.start_stream()
            assert result is False
            assert pipeline.state == PipelineState.ERROR

    @pytest.mark.asyncio
    async def test_start_stream_no_encoder(self):
        from ados.hal.camera import CameraInfo, CameraType

        cam = CameraInfo(name="Test", type=CameraType.CSI, device_path="/dev/video0")
        config = VideoConfig()
        pipeline = VideoPipeline(config)

        with (
            patch("ados.services.video.pipeline.discover_cameras", return_value=[cam]),
            patch("ados.services.video.pipeline.detect_encoder_for_camera", return_value=None),
        ):
            result = await pipeline.start_stream()
            assert result is False
            assert pipeline.state == PipelineState.ERROR

    @pytest.mark.asyncio
    async def test_stop_stream_when_not_running(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        await pipeline.stop_stream()
        assert pipeline.state == PipelineState.STOPPED

    @pytest.mark.asyncio
    async def test_check_health_no_process(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        assert await pipeline._check_health() is False

    @pytest.mark.asyncio
    async def test_check_health_process_exited(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        mock_proc = MagicMock()
        mock_proc.returncode = 1
        pipeline._encoder_process = mock_proc
        assert await pipeline._check_health() is False

    @pytest.mark.asyncio
    async def test_check_health_process_running(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        mock_proc = MagicMock()
        mock_proc.returncode = None
        # Use the test process pid so os.kill(pid, 0) succeeds
        mock_proc.pid = os.getpid()
        pipeline._encoder_process = mock_proc
        # Pretend mediamtx is alive and within startup grace period
        pipeline._mediamtx = MagicMock()
        pipeline._mediamtx.is_running = MagicMock(return_value=True)
        import time as _time
        pipeline._started_at = _time.monotonic()
        assert await pipeline._check_health() is True

    @pytest.mark.asyncio
    async def test_start_stream_already_running(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        pipeline._state = PipelineState.RUNNING
        result = await pipeline.start_stream()
        assert result is True
