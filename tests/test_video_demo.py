"""Tests for the demo video pipeline."""

from __future__ import annotations

import asyncio

import pytest

from ados.services.video.demo import DemoVideoPipeline


class TestDemoVideoPipeline:
    def test_initial_state(self):
        demo = DemoVideoPipeline()
        assert demo.streaming is False
        assert demo.recording is False

    def test_cameras_auto_assigned(self):
        demo = DemoVideoPipeline()
        mgr = demo.camera_manager
        assert len(mgr.cameras) == 2
        primary = mgr.get_primary()
        assert primary is not None
        assert "CSI" in primary.name

    def test_start_stop_stream(self):
        demo = DemoVideoPipeline()
        assert demo.start_stream() is True
        assert demo.streaming is True
        demo.stop_stream()
        assert demo.streaming is False

    def test_start_stop_recording(self):
        demo = DemoVideoPipeline()
        path = demo.start_recording()
        assert demo.recording is True
        assert path != ""
        stop_path = demo.stop_recording()
        assert stop_path == path
        assert demo.recording is False

    def test_capture_snapshot(self):
        demo = DemoVideoPipeline()
        p1 = demo.capture_snapshot()
        p2 = demo.capture_snapshot()
        assert p1 != p2
        assert p1.endswith(".jpg")
        assert "0001" in p1
        assert "0002" in p2

    def test_get_status(self):
        demo = DemoVideoPipeline()
        status = demo.get_status()
        assert status["state"] == "stopped"
        assert status["encoder"] == "demo"
        assert status["demo"] is True
        assert "cameras" in status
        assert "recorder" in status
        assert "mediamtx" in status

    def test_get_status_after_stream_start(self):
        demo = DemoVideoPipeline()
        demo.start_stream()
        status = demo.get_status()
        assert status["state"] == "running"

    @pytest.mark.asyncio
    async def test_run_can_be_cancelled(self):
        demo = DemoVideoPipeline()
        task = asyncio.create_task(demo.run())
        await asyncio.sleep(0.1)
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass
        # Stream was set to True by run()
        assert demo.streaming is True
