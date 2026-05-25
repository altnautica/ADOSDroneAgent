"""Tests for HopListener._maybe_follow_peer_channel gating.

Hearing a peer's presence beacon means we are already on the right
control-plane channel, so following the announced channel blindly can
retune AWAY from a working link. The follow must only fire when we are
NOT decoding valid video (valid-rx rate is zero), must skip when an
acquisition is already in flight, and must skip when already on-channel.
"""

from __future__ import annotations

import asyncio
from unittest.mock import MagicMock

import pytest

from ados.services.wfb.hop_supervisor import HopListener


def _make_listener(*, current_channel, valid_rate, acquirer_in_progress):
    """A HopListener wired to a fake wfb manager with a fake acquirer."""
    wfb = MagicMock()
    wfb._channel = current_channel
    wfb._valid_rx_packets_per_s = valid_rate
    acquirer = MagicMock()
    acquirer.in_progress = acquirer_in_progress
    acquirer.acquire_target = MagicMock()  # should NOT be scheduled in skip cases
    wfb._acquirer = acquirer
    wfb._persist_locked_channel = MagicMock()
    listener = HopListener(wfb_manager=wfb)
    return listener, wfb, acquirer


@pytest.mark.asyncio
async def test_skip_follow_when_decoding_video():
    """Decoding valid video → do not retune away from the working channel."""
    listener, wfb, acquirer = _make_listener(
        current_channel=149, valid_rate=120.0, acquirer_in_progress=False
    )
    listener._maybe_follow_peer_channel(44)
    # Let any (erroneously) scheduled task run.
    await asyncio.sleep(0)
    acquirer.acquire_target.assert_not_called()


@pytest.mark.asyncio
async def test_skip_follow_when_already_on_channel():
    """Announced channel == current → nothing to do."""
    listener, wfb, acquirer = _make_listener(
        current_channel=44, valid_rate=0.0, acquirer_in_progress=False
    )
    listener._maybe_follow_peer_channel(44)
    await asyncio.sleep(0)
    acquirer.acquire_target.assert_not_called()


@pytest.mark.asyncio
async def test_skip_follow_when_acquisition_in_flight():
    """A sweep already holds the acquirer lock → do not pile on."""
    listener, wfb, acquirer = _make_listener(
        current_channel=149, valid_rate=0.0, acquirer_in_progress=True
    )
    listener._maybe_follow_peer_channel(44)
    await asyncio.sleep(0)
    acquirer.acquire_target.assert_not_called()


@pytest.mark.asyncio
async def test_follow_fires_when_not_decoding_and_off_channel():
    """Not decoding + off-channel + idle acquirer → beacon-guided lock."""
    listener, wfb, acquirer = _make_listener(
        current_channel=149, valid_rate=0.0, acquirer_in_progress=False
    )

    # acquire_target must be awaitable and report success.
    async def _target(_ch):
        return True

    acquirer.acquire_target = _target
    listener._maybe_follow_peer_channel(44)
    # The verify is scheduled as a background task; give it a couple of
    # loop turns to run.
    await asyncio.sleep(0)
    await asyncio.sleep(0)
    assert wfb._channel == 44
    wfb._persist_locked_channel.assert_called_with(44)
