"""Tests for the ffmpeg wchar-based frame-stall watchdog on the ground profile.

The watchdog reads ``/proc/<ffmpeg_pid>/io`` and treats the cumulative
``wchar`` counter (kernel-side write() bytes) as the primary liveness
signal. The stderr-frame parser remains as a fallback when /proc is
gated.

Three coverage cases:

  1. wchar advances between probes → no stall.
  2. wchar flat for the whole window → stall declared.
  3. wchar unreadable (returns ``None``) → falls back to the
     frame-counter path.
"""

from __future__ import annotations

import time
from unittest.mock import MagicMock, patch

from ados.services.ground_station.mediamtx.manager import MediamtxGsManager


def _make_manager_with_alive_ffmpeg() -> MediamtxGsManager:
    mgr = MediamtxGsManager()
    fake_proc = MagicMock()
    fake_proc.pid = 9999
    fake_proc.returncode = None
    mgr._ffmpeg = fake_proc
    return mgr


def test_wchar_advancing_is_not_stalled() -> None:
    mgr = _make_manager_with_alive_ffmpeg()
    now = time.monotonic()
    mgr._ffmpeg_last_frame_at = now
    mgr._ffmpeg_frame_count = 100

    # First call seeds the baseline; allow the cold-start grace path.
    with patch.object(mgr, "_read_ffmpeg_wchar", side_effect=[1_000_000, 2_000_000]):
        first = mgr.ffmpeg_frame_stalled(window_s=8.0)
        second = mgr.ffmpeg_frame_stalled(window_s=8.0)

    # First call returns false because it's within the cold-start grace
    # window (since_start < 28s). Second call sees wchar advance and
    # explicitly returns False.
    assert first is False
    assert second is False


def test_wchar_flat_past_window_is_stalled() -> None:
    mgr = _make_manager_with_alive_ffmpeg()
    # Seed: last wchar sample was 30 s ago and hasn't moved since.
    mgr._ffmpeg_last_wchar = 5_000_000
    mgr._ffmpeg_last_wchar_at = time.monotonic() - 30.0
    mgr._ffmpeg_last_frame_at = time.monotonic()
    mgr._ffmpeg_frame_count = 200

    with patch.object(mgr, "_read_ffmpeg_wchar", return_value=5_000_000):
        assert mgr.ffmpeg_frame_stalled(window_s=8.0) is True


def test_wchar_unreadable_falls_back_to_frame_counter() -> None:
    mgr = _make_manager_with_alive_ffmpeg()
    # No frames yet + frame_at older than 28 s cold-start grace.
    mgr._ffmpeg_frame_count = 0
    mgr._ffmpeg_last_frame_at = time.monotonic() - 30.0

    with patch.object(mgr, "_read_ffmpeg_wchar", return_value=None):
        # Falls through to the cold-start branch; 30 s elapsed > 28 s.
        assert mgr.ffmpeg_frame_stalled(window_s=8.0) is True


def test_wchar_unreadable_with_recent_frame_is_not_stalled() -> None:
    mgr = _make_manager_with_alive_ffmpeg()
    mgr._ffmpeg_frame_count = 150
    mgr._ffmpeg_last_frame_at = time.monotonic()  # frame just arrived

    with patch.object(mgr, "_read_ffmpeg_wchar", return_value=None):
        assert mgr.ffmpeg_frame_stalled(window_s=8.0) is False


def test_dead_ffmpeg_is_not_stalled() -> None:
    """ffmpeg_alive() False short-circuits before the wchar path."""
    mgr = MediamtxGsManager()
    mgr._ffmpeg = None
    assert mgr.ffmpeg_frame_stalled(window_s=8.0) is False
