"""Tests for the ground-side ingest watchdog.

The ground station used to be able to go dark FOREVER in two ways, and both are
pinned here:

  1. **A wedged-but-alive ffmpeg.** ``ffmpeg_alive()`` (``returncode is None``)
     was the only liveness test, because the previous stall watchdog
     false-positived on a healthy process and was disabled in a long comment.
     A wedged ingest therefore held the ``/main`` path with a live PID, pushed
     nothing, and froze the operator's video indefinitely. Process liveness is
     never proof of work.
  2. **A dead mediamtx core behind a live ffmpeg.** The core-liveness probe sat
     INSIDE the ``if not ffmpeg_alive()`` branch, so mediamtx OOMing while
     ffmpeg stayed blocked on a half-open RTSP socket left nothing bound to
     8554 and nothing restarting it. The branch never ran.

The liveness signal is now ffmpeg's ``-progress pipe:2`` block's cumulative
``total_size=N`` output-byte counter, corroborated by mediamtx's own
``bytesReceived`` delta on the far side of the RTSP socket.
"""

from __future__ import annotations

import asyncio
import time
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.services.ground_station.mediamtx.ffmpeg_monitor import (
    FFMPEG_FIRST_OUTPUT_GRACE_SECONDS,
    FFMPEG_OUTPUT_STALL_SECONDS,
    drain_ffmpeg_stderr,
)
from ados.services.ground_station.mediamtx.manager import MediamtxGsManager
from ados.services.ground_station.mediamtx.process_argv import (
    build_ffmpeg_ingest_argv,
)
from ados.services.ground_station.mediamtx.tx_watchdog import monitor_ffmpeg


def _make_manager_with_alive_ffmpeg() -> MediamtxGsManager:
    mgr = MediamtxGsManager()
    fake_proc = MagicMock()
    fake_proc.pid = 9999
    fake_proc.returncode = None
    mgr._ffmpeg = fake_proc
    return mgr


# ---------------------------------------------------------------------------
# The argv carries the signal at all
# ---------------------------------------------------------------------------


def test_ingest_argv_requests_a_flushed_progress_block() -> None:
    """Without ``-progress pipe:2`` there is no reliable signal to watch.

    ffmpeg suppresses its status line entirely when stderr is not a tty and
    block-buffers stderr behind a subprocess pipe, which is exactly why the
    old ``frame=`` parser starved on a healthy process.
    """
    argv = build_ffmpeg_ingest_argv(
        "ffmpeg", __import__("pathlib").Path("/tmp/v.sdp"), "rtsp://127.0.0.1:8554/main"
    )
    assert "-progress" in argv
    assert argv[argv.index("-progress") + 1] == "pipe:2"


# ---------------------------------------------------------------------------
# 1. The live-but-silent process
# ---------------------------------------------------------------------------


def test_a_live_but_silent_ffmpeg_is_stalled() -> None:
    """THE CASE THAT USED TO FREEZE VIDEO FOREVER.

    The process is alive, it has produced output before, and its output-byte
    counter has stopped advancing. That is a wedged ingest holding /main.
    """
    mgr = _make_manager_with_alive_ffmpeg()
    mgr._ffmpeg_started_at = time.monotonic() - 120.0
    # It got as far as writing real output...
    mgr._ffmpeg_output_bytes = 5_000_000
    # ...and then stopped, well past the window.
    mgr._ffmpeg_output_advanced_at = time.monotonic() - (
        FFMPEG_OUTPUT_STALL_SECONDS + 5.0
    )

    assert mgr.ffmpeg_alive() is True, "the premise: the process is NOT dead"
    assert mgr.ffmpeg_output_stalled() is True


def test_an_advancing_output_counter_is_healthy() -> None:
    mgr = _make_manager_with_alive_ffmpeg()
    mgr._ffmpeg_started_at = time.monotonic() - 120.0
    mgr._ffmpeg_output_bytes = 5_000_000
    mgr._ffmpeg_output_advanced_at = time.monotonic()

    assert mgr.ffmpeg_output_stalled() is False


def test_output_flat_but_inside_the_window_is_not_yet_a_stall() -> None:
    """One missed progress flush must not reap a working ingest."""
    mgr = _make_manager_with_alive_ffmpeg()
    mgr._ffmpeg_started_at = time.monotonic() - 120.0
    mgr._ffmpeg_output_bytes = 5_000_000
    mgr._ffmpeg_output_advanced_at = time.monotonic() - (
        FFMPEG_OUTPUT_STALL_SECONDS - 1.0
    )

    assert mgr.ffmpeg_output_stalled() is False


def test_no_output_yet_inside_the_startup_grace_is_not_a_stall() -> None:
    """Nothing is expected on the output side during the RTSP handshake, the
    first-IDR wait and the 20 s SPS/PPS probe window. Tripping there is the
    false-positive class that got the previous watchdog disabled."""
    mgr = _make_manager_with_alive_ffmpeg()
    mgr._ffmpeg_started_at = time.monotonic() - (
        FFMPEG_FIRST_OUTPUT_GRACE_SECONDS - 5.0
    )
    mgr._ffmpeg_output_bytes = -1

    assert mgr.ffmpeg_output_stalled() is False


def test_no_output_at_all_past_the_grace_is_a_stall() -> None:
    mgr = _make_manager_with_alive_ffmpeg()
    mgr._ffmpeg_started_at = time.monotonic() - (
        FFMPEG_FIRST_OUTPUT_GRACE_SECONDS + 5.0
    )
    mgr._ffmpeg_output_bytes = -1

    assert mgr.ffmpeg_output_stalled() is True


def test_dead_ffmpeg_is_not_reported_as_stalled() -> None:
    """The dead-process branch owns that case; this check must not double-fire."""
    mgr = MediamtxGsManager()
    mgr._ffmpeg = None
    assert mgr.ffmpeg_output_stalled() is False


def test_a_respawn_reset_does_not_read_as_flat() -> None:
    """``total_size`` restarts from 0 on a respawn, so a lower value re-seeds
    the high-water mark instead of counting as forward progress — and must not
    leave the fresh process looking already-stalled."""
    mgr = _make_manager_with_alive_ffmpeg()
    mgr._ffmpeg_started_at = time.monotonic()
    mgr._ffmpeg_output_bytes = 9_000_000
    mgr._ffmpeg_output_advanced_at = time.monotonic()

    mgr._record_output_bytes(1024)
    assert mgr.ffmpeg_output_bytes() == 1024
    mgr._record_output_bytes(4096)
    assert mgr.ffmpeg_output_bytes() == 4096
    assert mgr.ffmpeg_output_stalled() is False


# ---------------------------------------------------------------------------
# The stderr drain must actually surface total_size
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_the_drain_parses_total_size_from_newline_delimited_progress() -> None:
    """``-progress pipe:2`` emits NEWLINE-terminated ``key=value`` lines.

    The previous drain used ``readuntil(b"\\r")``, which under ``-progress``
    finds no separator at all and only yields once the 64 KB StreamReader
    limit overruns — at ~200 bytes per block that is minutes of delay on the
    one signal the watchdog depends on.
    """
    block = (
        b"frame=120\nfps=30.0\nbitrate=4000.0kbits/s\ntotal_size=1048576\n"
        b"out_time_ms=4000000\nprogress=continue\n"
        b"frame=150\nfps=30.0\ntotal_size=2097152\nprogress=continue\n"
    )

    reader = asyncio.StreamReader()
    reader.feed_data(block)
    reader.feed_eof()
    proc = MagicMock()
    proc.stderr = reader

    seen: list[int] = []
    await drain_ffmpeg_stderr(proc, seen.append)

    assert seen == [1048576, 2097152]


@pytest.mark.asyncio
async def test_the_drain_still_surfaces_carriage_return_diagnostics() -> None:
    """Real ffmpeg errors use ``\\r`` and must still reach the log path, so the
    splitter has to handle both separators rather than swapping one for the
    other."""
    reader = asyncio.StreamReader()
    reader.feed_data(b"total_size=64\r[rtsp @ 0x1] Could not write header\r")
    reader.feed_eof()
    proc = MagicMock()
    proc.stderr = reader

    seen: list[int] = []
    with patch(
        "ados.services.ground_station.mediamtx.ffmpeg_monitor.log"
    ) as mock_log:
        await drain_ffmpeg_stderr(proc, seen.append)

    assert seen == [64]
    warned = " ".join(str(c) for c in mock_log.warning.call_args_list)
    assert "Could not write header" in warned


# ---------------------------------------------------------------------------
# 2. A dead mediamtx core behind a live ffmpeg
# ---------------------------------------------------------------------------


def _watchdog_manager(*, core_alive: bool, ffmpeg_alive: bool) -> MagicMock:
    """A manager double for the monitor loop."""
    mgr = MagicMock()
    mgr.core_alive.return_value = core_alive
    mgr.ffmpeg_alive.return_value = ffmpeg_alive
    mgr.restart_core = AsyncMock(return_value=True)
    mgr.restart_ffmpeg = AsyncMock(return_value=True)
    mgr.stop_ffmpeg_ingest = AsyncMock()
    mgr.path_has_publisher = AsyncMock(return_value=True)
    mgr.path_inbound_bytes = AsyncMock(return_value=1000)
    mgr.ffmpeg_output_stalled.return_value = False
    mgr.ffmpeg_output_bytes.return_value = 1000
    return mgr


async def _one_tick(mgr: MagicMock) -> None:
    """Run the monitor for exactly one tick, then shut it down.

    The tick interval is patched to near-zero so the test does not wait on the
    real 2 s cadence.
    """
    shutdown = asyncio.Event()
    with patch(
        "ados.services.ground_station.mediamtx.tx_watchdog"
        ".FFMPEG_MONITOR_TICK_SECONDS",
        0.01,
    ):
        task = asyncio.create_task(monitor_ffmpeg(mgr, shutdown, MagicMock()))
        await asyncio.sleep(0.08)
        shutdown.set()
        try:
            await asyncio.wait_for(task, timeout=2.0)
        except TimeoutError:
            task.cancel()


@pytest.mark.asyncio
async def test_a_dead_core_behind_a_live_ffmpeg_is_detected_and_restarted() -> None:
    """THE OTHER CASE THAT USED TO GO DARK FOREVER.

    ffmpeg is alive (blocked on a half-open RTSP socket), so the old loop's
    ``if not ffmpeg_alive()`` branch never ran and the core probe nested inside
    it was unreachable. Nothing was bound to 8554 and nothing restarted it.
    """
    mgr = _watchdog_manager(core_alive=False, ffmpeg_alive=True)

    with patch(
        "ados.services.ground_station.mediamtx.tx_watchdog.wfb_source_signal",
        return_value="live",
    ):
        await _one_tick(mgr)

    assert mgr.restart_core.await_count >= 1, (
        "a dead mediamtx core must be restarted even while ffmpeg is alive"
    )
    # ffmpeg was pushing into a dead port, so it is reaped and respawned
    # against the fresh listener rather than left pointing at nothing.
    assert mgr.stop_ffmpeg_ingest.await_count >= 1


@pytest.mark.asyncio
async def test_a_live_core_is_not_restarted() -> None:
    mgr = _watchdog_manager(core_alive=True, ffmpeg_alive=True)

    with patch(
        "ados.services.ground_station.mediamtx.tx_watchdog.wfb_source_signal",
        return_value="live",
    ):
        await _one_tick(mgr)

    assert mgr.restart_core.await_count == 0


@pytest.mark.asyncio
async def test_a_stalled_ingest_is_reaped_when_the_radio_is_delivering() -> None:
    """The monitor must ACT on the stall signal, not merely compute it."""
    mgr = _watchdog_manager(core_alive=True, ffmpeg_alive=True)
    mgr.ffmpeg_output_stalled.return_value = True

    with patch(
        "ados.services.ground_station.mediamtx.tx_watchdog.wfb_source_signal",
        return_value="live",
    ):
        await _one_tick(mgr)

    assert mgr.stop_ffmpeg_ingest.await_count >= 1


@pytest.mark.asyncio
async def test_a_stalled_ingest_is_not_reaped_while_the_radio_is_silent() -> None:
    """Neither counter can advance with no source, so a flat counter there is
    not a fault — the no-source reaper owns that case on its own grace."""
    mgr = _watchdog_manager(core_alive=True, ffmpeg_alive=True)
    mgr.ffmpeg_output_stalled.return_value = True
    # A registered publisher keeps the no-source reaper from firing too, so the
    # only thing that could reap here is the stall check.
    mgr.path_has_publisher = AsyncMock(return_value=True)

    with patch(
        "ados.services.ground_station.mediamtx.tx_watchdog.wfb_source_signal",
        return_value="silent",
    ):
        await _one_tick(mgr)

    assert mgr.stop_ffmpeg_ingest.await_count == 0
    mgr.ffmpeg_output_stalled.assert_not_called()


@pytest.mark.asyncio
async def test_a_flat_mediamtx_inbound_counter_reaps_the_ingest() -> None:
    """The second, independent delta: ffmpeg can believe it is writing into a
    socket mediamtx is no longer draining."""
    mgr = _watchdog_manager(core_alive=True, ffmpeg_alive=True)
    # Same value on every sample → flat.
    mgr.path_inbound_bytes = AsyncMock(return_value=4242)

    shutdown = asyncio.Event()
    with (
        patch(
            "ados.services.ground_station.mediamtx.tx_watchdog"
            ".FFMPEG_MONITOR_TICK_SECONDS",
            0.01,
        ),
        patch(
            "ados.services.ground_station.mediamtx.tx_watchdog"
            ".INBOUND_STALL_SECONDS",
            0.02,
        ),
        patch(
            "ados.services.ground_station.mediamtx.tx_watchdog.wfb_source_signal",
            return_value="live",
        ),
    ):
        task = asyncio.create_task(monitor_ffmpeg(mgr, shutdown, MagicMock()))
        await asyncio.sleep(0.15)
        shutdown.set()
        try:
            await asyncio.wait_for(task, timeout=2.0)
        except TimeoutError:
            task.cancel()

    assert mgr.stop_ffmpeg_ingest.await_count >= 1


@pytest.mark.asyncio
async def test_an_unreachable_mediamtx_api_is_not_treated_as_a_stall() -> None:
    """``None`` is not evidence about byte flow. The core probe at the top of
    the tick owns "is mediamtx there at all"; conflating the two would reap a
    healthy ingest on a transient API timeout."""
    mgr = _watchdog_manager(core_alive=True, ffmpeg_alive=True)
    mgr.path_inbound_bytes = AsyncMock(return_value=None)

    shutdown = asyncio.Event()
    with (
        patch(
            "ados.services.ground_station.mediamtx.tx_watchdog"
            ".FFMPEG_MONITOR_TICK_SECONDS",
            0.01,
        ),
        patch(
            "ados.services.ground_station.mediamtx.tx_watchdog"
            ".INBOUND_STALL_SECONDS",
            0.02,
        ),
        patch(
            "ados.services.ground_station.mediamtx.tx_watchdog.wfb_source_signal",
            return_value="live",
        ),
    ):
        task = asyncio.create_task(monitor_ffmpeg(mgr, shutdown, MagicMock()))
        await asyncio.sleep(0.15)
        shutdown.set()
        try:
            await asyncio.wait_for(task, timeout=2.0)
        except TimeoutError:
            task.cancel()

    assert mgr.stop_ffmpeg_ingest.await_count == 0
