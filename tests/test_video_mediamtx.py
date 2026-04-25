"""Tests for the mediamtx subprocess manager."""

from __future__ import annotations

import tempfile
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.services.video.mediamtx import MediamtxManager


class TestMediamtxManager:
    def test_initial_state(self):
        mgr = MediamtxManager()
        assert mgr.running is False
        assert mgr.config_path == ""
        assert mgr.rtsp_port == 8554
        assert mgr.webrtc_port == 8889

    def test_custom_ports(self):
        mgr = MediamtxManager(api_port=9000, rtsp_port=9001, webrtc_port=9002)
        assert mgr.rtsp_port == 9001
        assert mgr.webrtc_port == 9002

    def test_generate_config(self):
        mgr = MediamtxManager()
        streams = {"main": "rpicam-vid -o -", "thermal": "ffmpeg -i /dev/video2 -f mpegts -"}
        config_path = mgr.generate_config(streams)
        assert config_path != ""
        assert Path(config_path).exists()
        assert mgr.config_path == config_path

        # Verify YAML content
        import yaml

        with open(config_path) as f:
            config = yaml.safe_load(f)
        assert config["rtsp"] is True
        assert config["webrtc"] is True
        assert "main" in config["paths"]
        assert "thermal" in config["paths"]

        # Cleanup
        Path(config_path).unlink(missing_ok=True)

    def test_is_running_no_process(self):
        mgr = MediamtxManager()
        assert mgr.is_running() is False

    def test_is_running_process_exited(self):
        mgr = MediamtxManager()
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mgr._process = mock_proc
        mgr._running = True
        assert mgr.is_running() is False

    def test_is_running_process_alive(self):
        mgr = MediamtxManager()
        mock_proc = MagicMock()
        mock_proc.returncode = None
        mgr._process = mock_proc
        mgr._running = True
        assert mgr.is_running() is True

    def test_to_dict(self):
        mgr = MediamtxManager()
        d = mgr.to_dict()
        assert d["running"] is False
        assert d["rtsp_port"] == 8554
        assert d["webrtc_port"] == 8889

    @pytest.mark.asyncio
    async def test_start_no_binary(self):
        mgr = MediamtxManager()
        mgr._config_path = "/tmp/test.yml"
        with patch("ados.services.video.mediamtx.shutil.which", return_value=None):
            result = await mgr.start()
            assert result is False

    @pytest.mark.asyncio
    async def test_start_no_config(self):
        mgr = MediamtxManager()
        with patch("ados.services.video.mediamtx.shutil.which", return_value="/usr/bin/mediamtx"):
            result = await mgr.start()
            assert result is False

    @pytest.mark.asyncio
    async def test_start_already_running(self):
        mgr = MediamtxManager()
        mgr._running = True
        mock_proc = MagicMock()
        mock_proc.returncode = None
        mgr._process = mock_proc
        result = await mgr.start()
        assert result is True

    @pytest.mark.asyncio
    async def test_stop_when_not_running(self):
        mgr = MediamtxManager()
        await mgr.stop()  # Should not raise
        assert mgr.running is False

    @pytest.mark.asyncio
    async def test_stop_cleans_config(self):
        mgr = MediamtxManager()
        mock_proc = AsyncMock()
        mock_proc.terminate = MagicMock()
        mock_proc.wait = AsyncMock()
        mock_proc.returncode = 0
        mgr._process = mock_proc
        mgr._running = True

        # Create a temp config file to verify cleanup
        with tempfile.NamedTemporaryFile(suffix=".yml", delete=False) as f:
            mgr._config_path = f.name

        await mgr.stop()
        assert mgr.running is False
        assert not Path(mgr._config_path).exists() or mgr._config_path == ""
