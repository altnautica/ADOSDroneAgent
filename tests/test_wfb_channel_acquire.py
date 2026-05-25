"""Tests for the ground-side channel acquisition sweep.

The acquirer sweeps candidate channels and locks onto the first one
where the valid-decode counter (packets_received) advances within the
dwell. These tests drive a synthetic valid-packet counter so the lock /
no-lock decision is exercised without real radio hardware.
"""

from __future__ import annotations

import asyncio
from unittest.mock import patch

import pytest

from ados.services.wfb.channel_acquire import (
    AcquireState,
    ChannelAcquirer,
    candidate_channels,
)

_REAL_SLEEP = asyncio.sleep


@pytest.fixture(autouse=True)
def _instant_sleep():
    """Collapse the per-dwell sleeps so tests run in milliseconds."""

    async def _instant(_delay):
        await _REAL_SLEEP(0)

    with patch(
        "ados.services.wfb.channel_acquire.asyncio.sleep", side_effect=_instant
    ):
        yield


def test_candidate_channels_band_first():
    """U-NII-1 channels lead the sweep, with the rest appended."""
    chans = candidate_channels("u-nii-1")
    assert chans[:4] == [36, 40, 44, 48]
    # U-NII-3 channels still appear later so an out-of-band peer is found.
    assert 149 in chans
    assert len(chans) == 9


def test_candidate_channels_unknown_band_falls_back():
    chans = candidate_channels("nonsense")
    assert set(chans) == {36, 40, 44, 48, 149, 153, 157, 161, 165}


async def test_acquire_locks_on_first_decoding_channel():
    """Sweep locks on the channel where the valid counter advances.

    The synthetic counter stays flat for channels 36/40/44 and starts
    advancing once the acquirer tunes to channel 48, so the lock must
    land on 48.
    """
    state = {"channel": None, "value": 0}

    async def _fake_set(_iface, channel):
        state["channel"] = channel
        return True

    def _valid_packets():
        # Only channel 48 decodes valid packets; advance the counter
        # whenever we are tuned there.
        if state["channel"] == 48:
            state["value"] += 5
        return state["value"]

    acq = ChannelAcquirer(
        interface="wlan0",
        band="u-nii-1",
        valid_packets_fn=_valid_packets,
        set_channel_fn=_fake_set,
    )
    locked = await acq.acquire()
    assert locked == 48
    assert acq.state == AcquireState.LOCKED
    assert acq.channel_locked is True
    assert acq.locked_channel == 48


async def test_acquire_no_peer_when_nothing_decodes():
    """No channel decodes → status no-peer, returns None, bounded sweep."""

    async def _fake_set(_iface, _channel):
        return True

    def _valid_packets():
        return 0  # never advances

    acq = ChannelAcquirer(
        interface="wlan0",
        band="u-nii-1",
        valid_packets_fn=_valid_packets,
        set_channel_fn=_fake_set,
        max_sweep_rounds=2,
    )
    locked = await acq.acquire()
    assert locked is None
    assert acq.state == AcquireState.NO_PEER
    assert acq.channel_locked is False


async def test_acquire_target_verifies_announced_channel():
    """A beacon-announced channel that decodes locks with one dwell."""
    state = {"value": 0}

    async def _fake_set(_iface, channel):
        return True

    def _valid_packets():
        state["value"] += 3
        return state["value"]

    acq = ChannelAcquirer(
        interface="wlan0",
        band="u-nii-1",
        valid_packets_fn=_valid_packets,
        set_channel_fn=_fake_set,
    )
    ok = await acq.acquire_target(157)
    assert ok is True
    assert acq.locked_channel == 157
    assert acq.channel_locked is True


async def test_acquire_target_fails_when_silent():
    """An announced channel that does not decode does not lock."""

    async def _fake_set(_iface, _channel):
        return True

    def _valid_packets():
        return 7  # flat — no advance during the dwell

    acq = ChannelAcquirer(
        interface="wlan0",
        band="u-nii-1",
        valid_packets_fn=_valid_packets,
        set_channel_fn=_fake_set,
    )
    ok = await acq.acquire_target(161)
    assert ok is False
    assert acq.channel_locked is False


async def test_mark_unlocked_drops_a_lock():
    """mark_unlocked clears a prior lock back to searching."""
    state = {"value": 0}

    async def _fake_set(_iface, _channel):
        return True

    def _valid_packets():
        state["value"] += 2
        return state["value"]

    acq = ChannelAcquirer(
        interface="wlan0",
        band="u-nii-1",
        valid_packets_fn=_valid_packets,
        set_channel_fn=_fake_set,
    )
    await acq.acquire_target(149)
    assert acq.channel_locked is True
    acq.mark_unlocked()
    assert acq.channel_locked is False
    assert acq.state == AcquireState.SEARCHING


async def test_try_channel_skips_when_set_channel_fails():
    """A failed iw retune yields no lock for that channel."""

    async def _fake_set(_iface, _channel):
        return False

    def _valid_packets():
        return 100  # would advance, but set failed so dwell never runs

    acq = ChannelAcquirer(
        interface="wlan0",
        band="u-nii-1",
        valid_packets_fn=_valid_packets,
        set_channel_fn=_fake_set,
    )
    ok = await acq.try_channel(40)
    assert ok is False
    assert acq.channel_locked is False


async def test_acquire_and_target_are_mutually_exclusive():
    """A concurrent acquire() and acquire_target() never run together.

    The set_channel callback records whether more than one retune is in
    flight at once; the lock must serialize them so the max observed
    concurrency is 1.
    """
    inflight = {"now": 0, "max": 0}

    async def _fake_set(_iface, _channel):
        inflight["now"] += 1
        inflight["max"] = max(inflight["max"], inflight["now"])
        await _REAL_SLEEP(0)
        inflight["now"] -= 1
        return True

    def _valid_packets():
        return 0  # nothing ever decodes → both run to completion

    acq = ChannelAcquirer(
        interface="wlan0",
        band="u-nii-1",
        valid_packets_fn=_valid_packets,
        set_channel_fn=_fake_set,
        max_sweep_rounds=1,
    )
    await asyncio.gather(acq.acquire(), acq.acquire_target(149))
    assert inflight["max"] == 1


async def test_in_progress_flag_tracks_lock():
    """in_progress is False at rest and True while a retune holds the lock."""
    started = asyncio.Event()
    release = asyncio.Event()

    async def _fake_set(_iface, _channel):
        started.set()
        await release.wait()
        return True

    acq = ChannelAcquirer(
        interface="wlan0",
        band="u-nii-1",
        valid_packets_fn=lambda: 0,
        set_channel_fn=_fake_set,
    )
    assert acq.in_progress is False
    task = asyncio.create_task(acq.acquire_target(157))
    await started.wait()
    assert acq.in_progress is True
    release.set()
    await task
    assert acq.in_progress is False
