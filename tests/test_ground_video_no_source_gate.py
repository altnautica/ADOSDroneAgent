"""Tests for the ground-station video ingest's no-source gating.

On a ground station with no drone paired (or a drone powered off), the
radio receiver sits on a silent UDP port. ffmpeg spawned against that
port never finishes its codec probe: it neither exits nor registers a
publisher, and spins CPU on an idle appliance. Two mechanisms keep the
idle appliance ffmpeg-free while preserving the live path:

* ``wfb_source_signal()`` classifies the receiver as live / silent /
  unknown from the fresh wfb-stats snapshot.
* ``MediamtxGsManager.start()`` defers the ffmpeg spawn when the source
  is confirmed silent, and ``stop_ffmpeg_ingest()`` reaps a stuck-probe
  ffmpeg without tearing down the mediamtx core.
"""

from __future__ import annotations

import json
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.services.ground_station.mediamtx import tx_watchdog
from ados.services.ground_station.mediamtx.manager import MediamtxGsManager

_MGR_MODULE = "ados.services.ground_station.mediamtx.manager"


# ---------------------------------------------------------------------------
# wfb_source_signal
# ---------------------------------------------------------------------------


def _signal_with(age, payload):
    """Drive wfb_source_signal with a controlled freshness + snapshot body."""
    from unittest.mock import mock_open

    open_patch = (
        patch("builtins.open", side_effect=FileNotFoundError)
        if payload is None
        else patch("builtins.open", mock_open(read_data=json.dumps(payload)))
    )
    with patch.object(tx_watchdog, "_wfb_stats_age_seconds", return_value=age), open_patch:
        return tx_watchdog.wfb_source_signal()


def test_signal_live_when_fresh_and_packets_flowing():
    assert _signal_with(1.0, {"packets_received": 128}) == "live"


def test_signal_silent_when_fresh_but_zero_packets():
    assert _signal_with(1.0, {"packets_received": 0}) == "silent"


def test_signal_unknown_when_snapshot_stale():
    # A snapshot older than the freshness ceiling cannot prove there is
    # no source, so the caller must not defer on it.
    assert _signal_with(30.0, {"packets_received": 0}) == "unknown"


def test_signal_unknown_when_snapshot_missing():
    assert _signal_with(None, None) == "unknown"


def test_signal_unknown_when_packets_field_absent():
    assert _signal_with(1.0, {"acquire_state": "searching"}) == "unknown"


# ---------------------------------------------------------------------------
# MediamtxGsManager.start() deferral
# ---------------------------------------------------------------------------


def _prepared_manager():
    mgr = MediamtxGsManager()
    # Non-empty config path so start() skips generate_config().
    mgr._config_path = "/tmp/ados-test-mediamtx-gs.yml"
    mgr._core.start = AsyncMock(return_value=True)
    mgr._start_ffmpeg_ingest = AsyncMock(return_value=True)
    return mgr


@pytest.mark.asyncio
async def test_start_defers_ffmpeg_when_source_silent():
    mgr = _prepared_manager()
    with (
        patch(
            "ados.services.video.mediamtx._wait_for_tcp_port",
            AsyncMock(return_value=True),
        ),
        patch(f"{_MGR_MODULE}.wfb_source_signal", return_value="silent"),
    ):
        ok = await mgr.start()
    assert ok is True
    # Core is up and the service is marked running, but no ffmpeg probe
    # was spawned into the silent radio.
    assert mgr._running is True
    mgr._start_ffmpeg_ingest.assert_not_awaited()


@pytest.mark.asyncio
async def test_start_spawns_ffmpeg_when_source_live():
    mgr = _prepared_manager()
    with (
        patch(
            "ados.services.video.mediamtx._wait_for_tcp_port",
            AsyncMock(return_value=True),
        ),
        patch(f"{_MGR_MODULE}.wfb_source_signal", return_value="live"),
    ):
        ok = await mgr.start()
    assert ok is True
    assert mgr._running is True
    mgr._start_ffmpeg_ingest.assert_awaited_once()


@pytest.mark.asyncio
async def test_start_spawns_ffmpeg_when_source_unknown():
    # An absent / stale snapshot is inconclusive; preserve the boot-time
    # spawn so a live source that is slow to write stats is never dark-
    # held. A resulting stuck probe is reaped by the monitor loop.
    mgr = _prepared_manager()
    with (
        patch(
            "ados.services.video.mediamtx._wait_for_tcp_port",
            AsyncMock(return_value=True),
        ),
        patch(f"{_MGR_MODULE}.wfb_source_signal", return_value="unknown"),
    ):
        ok = await mgr.start()
    assert ok is True
    mgr._start_ffmpeg_ingest.assert_awaited_once()


# ---------------------------------------------------------------------------
# MediamtxGsManager.stop_ffmpeg_ingest()
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_stop_ffmpeg_ingest_reaps_alive_process_and_keeps_core():
    mgr = MediamtxGsManager()
    proc = AsyncMock()
    proc.terminate = MagicMock()
    proc.kill = MagicMock()
    proc.wait = AsyncMock()
    proc.returncode = None
    mgr._ffmpeg = proc
    # A core the reaper must NOT touch.
    mgr._core.stop = AsyncMock()

    await mgr.stop_ffmpeg_ingest()

    proc.terminate.assert_called_once()
    assert mgr._ffmpeg is None
    mgr._core.stop.assert_not_awaited()


@pytest.mark.asyncio
async def test_stop_ffmpeg_ingest_noop_when_no_process():
    mgr = MediamtxGsManager()
    mgr._ffmpeg = None
    await mgr.stop_ffmpeg_ingest()  # must not raise
    assert mgr._ffmpeg is None
