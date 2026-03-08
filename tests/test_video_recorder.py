"""Tests for the video recorder."""

from __future__ import annotations

import os
import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest

from ados.services.video.recorder import RecordingInfo, VideoRecorder


class TestRecordingInfo:
    def test_to_dict(self):
        info = RecordingInfo(
            filename="test.mp4",
            path="/tmp/test.mp4",
            size_bytes=1024,
            timestamp="2026-03-08T10:00:00+00:00",
            duration_seconds=30.0,
        )
        d = info.to_dict()
        assert d["filename"] == "test.mp4"
        assert d["size_bytes"] == 1024
        assert d["duration_seconds"] == 30.0


class TestVideoRecorder:
    def test_initial_state(self):
        rec = VideoRecorder("/tmp/ados/test-recordings")
        assert rec.recording is False
        assert rec.current_path == ""

    def test_generate_filename(self):
        rec = VideoRecorder()
        name = rec._generate_filename()
        assert name.startswith("recording_")
        assert name.endswith(".mp4")

    def test_to_dict(self):
        rec = VideoRecorder("/tmp/test")
        d = rec.to_dict()
        assert d["recording"] is False
        assert d["current_path"] == ""
        assert d["recordings_dir"] == "/tmp/test"

    def test_get_recordings_no_dir(self):
        rec = VideoRecorder("/nonexistent/path/recordings")
        assert rec.get_recordings() == []

    def test_get_recordings_with_files(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            # Create some fake recording files
            for name in ["recording_001.mp4", "recording_002.mp4", "notes.txt"]:
                (Path(tmpdir) / name).write_text("fake")

            rec = VideoRecorder(tmpdir)
            recordings = rec.get_recordings()
            # Should only list .mp4 files
            assert len(recordings) == 2
            assert all(r.filename.endswith(".mp4") for r in recordings)

    def test_ensure_dir_creates_directory(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            new_dir = os.path.join(tmpdir, "new", "recordings")
            rec = VideoRecorder(new_dir)
            rec._ensure_dir()
            assert Path(new_dir).is_dir()

    @pytest.mark.asyncio
    async def test_stop_recording_when_not_recording(self):
        rec = VideoRecorder()
        path = await rec.stop_recording()
        assert path == ""

    @pytest.mark.asyncio
    async def test_start_recording_ffmpeg_not_found(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            rec = VideoRecorder(tmpdir)
            with patch(
                "ados.services.video.recorder.asyncio.create_subprocess_exec",
                side_effect=FileNotFoundError,
            ):
                path = await rec.start_recording()
                assert path == ""
                assert rec.recording is False

    @pytest.mark.asyncio
    async def test_start_recording_already_active(self):
        rec = VideoRecorder()
        rec._recording = True
        rec._current_path = "/tmp/existing.mp4"
        path = await rec.start_recording()
        assert path == "/tmp/existing.mp4"
