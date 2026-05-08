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


def _pipeline_with_mediamtx_mocks():
    """Pipeline with camera + mediamtx fully mocked. Used for partial-start tests."""
    from ados.hal.camera import CameraInfo, CameraType

    config = VideoConfig()
    pipeline = VideoPipeline(config)
    cam = CameraInfo(name="Test", type=CameraType.CSI, device_path="/dev/video0")
    pipeline._discover_and_assign = MagicMock(
        side_effect=lambda: pipeline._camera_mgr.set_cameras([cam])
    )
    pipeline._camera_mgr.set_cameras = MagicMock()
    pipeline._camera_mgr.get_primary = MagicMock(return_value=cam)
    # Replace the manager wholesale so we can mock the read-only property too
    mtx = MagicMock()
    mtx.generate_config = MagicMock()
    mtx.start = AsyncMock(return_value=True)
    mtx.stop = AsyncMock(return_value=None)
    mtx.rtsp_port = 8554
    pipeline._mediamtx = mtx
    return pipeline


class TestPipelinePartialStartCleanup:
    """If post-mediamtx setup fails, mediamtx must be torn down so the next
    start_stream() does not collide on the port with a zombie."""

    @pytest.mark.asyncio
    async def test_filenotfound_during_encoder_spawn_stops_mediamtx(self, monkeypatch):
        from ados.services.video.encoder import EncoderType

        pipeline = _pipeline_with_mediamtx_mocks()
        monkeypatch.setattr(
            "ados.services.video.pipeline.detect_encoder_for_camera",
            lambda cam: EncoderType.FFMPEG,
        )
        monkeypatch.setattr(
            "ados.services.video.pipeline.build_encoder_command",
            lambda *a, **k: ["ffmpeg", "-i", "x", "y"],
        )

        async def boom(*args, **kwargs):
            raise FileNotFoundError("ffmpeg")

        import asyncio as _asyncio
        monkeypatch.setattr(_asyncio, "create_subprocess_exec", boom)

        ok = await pipeline.start_stream()
        assert ok is False
        assert pipeline.state == PipelineState.ERROR
        pipeline._mediamtx.stop.assert_awaited_once()

    @pytest.mark.asyncio
    async def test_oserror_during_encoder_spawn_stops_mediamtx(self, monkeypatch):
        from ados.services.video.encoder import EncoderType

        pipeline = _pipeline_with_mediamtx_mocks()
        monkeypatch.setattr(
            "ados.services.video.pipeline.detect_encoder_for_camera",
            lambda cam: EncoderType.FFMPEG,
        )
        monkeypatch.setattr(
            "ados.services.video.pipeline.build_encoder_command",
            lambda *a, **k: ["ffmpeg", "-i", "x", "y"],
        )

        async def boom(*args, **kwargs):
            raise OSError("permission denied")

        import asyncio as _asyncio
        monkeypatch.setattr(_asyncio, "create_subprocess_exec", boom)

        ok = await pipeline.start_stream()
        assert ok is False
        assert pipeline.state == PipelineState.ERROR
        pipeline._mediamtx.stop.assert_awaited_once()

    @pytest.mark.asyncio
    async def test_mediamtx_failed_to_start_does_not_call_stop(self, monkeypatch):
        from ados.services.video.encoder import EncoderType

        pipeline = _pipeline_with_mediamtx_mocks()
        pipeline._mediamtx.start = AsyncMock(return_value=False)
        monkeypatch.setattr(
            "ados.services.video.pipeline.detect_encoder_for_camera",
            lambda cam: EncoderType.FFMPEG,
        )
        monkeypatch.setattr(
            "ados.services.video.pipeline.build_encoder_command",
            lambda *a, **k: ["ffmpeg", "-i", "x", "y"],
        )

        ok = await pipeline.start_stream()
        assert ok is False
        assert pipeline.state == PipelineState.ERROR
        pipeline._mediamtx.stop.assert_not_awaited()


class TestRestartAttemptsSurface:
    """Public restart counter + healthy-window reset behaviour."""

    def test_restart_attempts_starts_at_zero(self):
        pipeline = VideoPipeline(VideoConfig())
        assert pipeline.restart_attempts() == 0

    def test_restart_attempts_reflects_internal_counter(self):
        pipeline = VideoPipeline(VideoConfig())
        pipeline._restart_count = 4
        assert pipeline.restart_attempts() == 4

    def test_first_healthy_tick_seeds_timer_without_clearing(self):
        pipeline = VideoPipeline(VideoConfig())
        pipeline._restart_count = 3
        cleared = pipeline._note_healthy_tick(now=100.0)
        # First call records the stamp but does not yet clear because
        # we have not measured a window of healthy time.
        assert cleared is False
        assert pipeline._last_healthy_at == 100.0
        assert pipeline._restart_count == 3

    def test_sustained_healthy_window_clears_counter(self):
        pipeline = VideoPipeline(VideoConfig())
        pipeline._restart_count = 5
        pipeline._note_healthy_tick(now=100.0)
        # 30 s in: still inside the window, counter intact.
        assert pipeline._note_healthy_tick(now=130.0) is False
        assert pipeline._restart_count == 5
        # 65 s in: past the 60 s window, counter clears.
        assert pipeline._note_healthy_tick(now=165.0) is True
        assert pipeline._restart_count == 0

    def test_no_clear_when_counter_already_zero(self):
        pipeline = VideoPipeline(VideoConfig())
        pipeline._note_healthy_tick(now=100.0)
        # Past the window with a clean counter — nothing to clear.
        assert pipeline._note_healthy_tick(now=200.0) is False
        assert pipeline._restart_count == 0

    def test_unhealthy_tick_resets_window(self):
        pipeline = VideoPipeline(VideoConfig())
        pipeline._restart_count = 4
        pipeline._note_healthy_tick(now=100.0)
        # Failure interrupts the streak.
        pipeline._note_unhealthy_tick()
        assert pipeline._last_healthy_at == 0.0
        # Counter has not been touched by the unhealthy tick itself.
        assert pipeline._restart_count == 4
        # New healthy stamp at t=200; another window must elapse before
        # the counter can clear (test that we did not retain the old
        # 100.0 stamp somehow).
        pipeline._note_healthy_tick(now=200.0)
        assert pipeline._last_healthy_at == 200.0
        # 30 s later, still within window — no clear.
        assert pipeline._note_healthy_tick(now=230.0) is False
        assert pipeline._restart_count == 4
        # 65 s after the new stamp, clear fires.
        assert pipeline._note_healthy_tick(now=270.0) is True
        assert pipeline._restart_count == 0
