"""Tests for the ffmpeg gate's awareness of channel acquisition.

The ground ffmpeg ingest gate holds ffmpeg while no valid packets are
flowing. It now reads the acquisition state from the shared wfb-stats
snapshot so it can emit an actionable status (searching vs no-peer)
instead of a blind silent hold, and so it starts ffmpeg as soon as
valid packets flow.
"""

from __future__ import annotations

import json
from unittest.mock import mock_open, patch

from ados.services.ground_station.mediamtx import tx_watchdog


def _patch_stats(payload):
    blob = json.dumps(payload)
    return patch("builtins.open", mock_open(read_data=blob))


def test_acquire_state_read_from_stats():
    with _patch_stats({"packets_received": 0, "acquire_state": "searching"}):
        assert tx_watchdog._wfb_acquire_state() == "searching"


def test_acquire_state_defaults_idle_when_absent():
    with _patch_stats({"packets_received": 0}):
        assert tx_watchdog._wfb_acquire_state() == "idle"


def test_acquire_state_idle_when_file_missing():
    with patch("builtins.open", side_effect=FileNotFoundError):
        assert tx_watchdog._wfb_acquire_state() == "idle"


def test_packets_received_still_read():
    with _patch_stats({"packets_received": 42, "acquire_state": "locked"}):
        assert tx_watchdog._wfb_packets_received() == 42


def test_packets_received_none_when_field_absent():
    with _patch_stats({"acquire_state": "no-peer"}):
        assert tx_watchdog._wfb_packets_received() is None
