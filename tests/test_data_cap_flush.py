"""Tests for DataCapTracker flush-on-stop and fsync durability.

Atomic write (tmp + os.replace) was already in place. The remaining gap
was that a graceful shutdown lost the bytes accumulated since the last
60-second poll, and the absence of fsync left the file vulnerable to
power loss between write() and the kernel flushing the page cache.
"""

from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.services.ground_station.uplink_router import (
    DataCapTracker,
    UplinkEventBus,
)


def _tracker(tmp_path: Path) -> tuple[DataCapTracker, MagicMock]:
    modem = MagicMock()
    modem.data_usage = AsyncMock(return_value={"rx_bytes": 0, "tx_bytes": 0})
    bus = UplinkEventBus()
    state_path = tmp_path / "modem-usage.json"
    return DataCapTracker(modem, bus, cap_gb=1.0, state_path=state_path), modem


@pytest.mark.asyncio
async def test_stop_flushes_state_to_disk(tmp_path: Path):
    """A clean stop must persist the latest counter, not lose the
    bytes accumulated since the last poll."""
    tracker, _modem = _tracker(tmp_path)

    # Mutate state without going through _save_state (simulate bytes
    # observed mid-poll-window)
    tracker._state.cumulative_bytes = 9_999_999

    # Initially, the on-disk file does not reflect this (file may not
    # exist yet)
    state_path = tracker._state_path
    assert (
        not state_path.exists()
        or json.loads(state_path.read_text())["cumulative_bytes"] == 0
    )

    # Graceful stop must flush
    await tracker.stop()
    persisted = json.loads(state_path.read_text())
    assert persisted["cumulative_bytes"] == 9_999_999


@pytest.mark.asyncio
async def test_stop_flush_failure_is_swallowed(tmp_path: Path):
    """A flush failure during stop must not raise — shutdown must complete."""
    tracker, _modem = _tracker(tmp_path)
    tracker._state.cumulative_bytes = 1234

    # Force the save to fail
    with patch.object(
        tracker, "_save_state", side_effect=OSError("disk full")
    ):
        # Must not raise
        await tracker.stop()


def test_save_state_calls_fsync(tmp_path: Path):
    """The atomic write must fsync the temp file before rename so the
    bytes survive a power cut between write() and the kernel page-cache
    flush."""
    tracker, _modem = _tracker(tmp_path)
    tracker._state.cumulative_bytes = 42

    fsync_called = []
    real_fsync = os.fsync

    def spy_fsync(fd):
        fsync_called.append(fd)
        return real_fsync(fd)

    with patch("ados.services.ground_station.uplink.data_cap.os.fsync", spy_fsync):
        tracker._save_state()

    assert fsync_called, "_save_state must fsync the temp file before rename"


def test_save_state_writes_atomically(tmp_path: Path):
    """tmp file + os.replace pattern: a partially-written tmp must never
    appear at the canonical path even if the write is interrupted."""
    tracker, _modem = _tracker(tmp_path)
    tracker._state.cumulative_bytes = 5000

    real_replace = os.replace
    rename_calls: list[tuple[str, str]] = []

    def spy_replace(src, dst):
        rename_calls.append((str(src), str(dst)))
        return real_replace(src, dst)

    with patch("ados.services.ground_station.uplink.data_cap.os.replace", spy_replace):
        tracker._save_state()

    assert len(rename_calls) == 1
    src, dst = rename_calls[0]
    assert src.endswith(".json.tmp")
    assert dst.endswith("modem-usage.json")
    # The tmp file must no longer exist after replace
    assert not Path(src).exists()


@pytest.mark.asyncio
async def test_stop_when_never_started_still_flushes(tmp_path: Path):
    """stop() must be safe to call even if start() was never called.
    Useful when a service crashes before its task gets scheduled."""
    tracker, _modem = _tracker(tmp_path)
    tracker._state.cumulative_bytes = 7

    # Never called start, so _task is None
    assert tracker._task is None
    await tracker.stop()

    persisted = json.loads(tracker._state_path.read_text())
    assert persisted["cumulative_bytes"] == 7
