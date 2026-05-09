"""Tests for `select_quietest_channel` band-filtered selection."""

from __future__ import annotations

from unittest import mock

from ados.services.wfb import channel as channel_mod


def _build_scan_results():
    """Synthesise a scan_channels() return value that makes the band
    test deterministic: ch 36 quiet, ch 149 loud, others moderate."""
    return [
        (channel_mod._CHANNEL_MAP[36], 0),
        (channel_mod._CHANNEL_MAP[44], 1),
        (channel_mod._CHANNEL_MAP[149], 12),
        (channel_mod._CHANNEL_MAP[157], 8),
    ]


def test_select_quietest_channel_unii1_picks_lowest_in_band():
    with mock.patch.object(
        channel_mod, "scan_channels", return_value=_build_scan_results()
    ):
        ch = channel_mod.select_quietest_channel("wlan1", band="u-nii-1")
    assert ch.channel_number in (36, 40, 44, 48)
    # Specifically ch 36 has the lowest count
    assert ch.channel_number == 36


def test_select_quietest_channel_unii3_filters_out_unii1():
    with mock.patch.object(
        channel_mod, "scan_channels", return_value=_build_scan_results()
    ):
        ch = channel_mod.select_quietest_channel("wlan1", band="u-nii-3")
    assert ch.channel_number in (149, 153, 157, 161, 165)
    # 157 has count 8, 149 has count 12 — picker prefers lower
    assert ch.channel_number == 157


def test_select_quietest_channel_all_band_uses_full_set():
    with mock.patch.object(
        channel_mod, "scan_channels", return_value=_build_scan_results()
    ):
        ch = channel_mod.select_quietest_channel("wlan1", band="all")
    # Best across the full set is still 36
    assert ch.channel_number == 36


def test_select_quietest_channel_falls_back_when_band_filter_empty():
    """If the scan returned only U-NII-3 channels but the band asked
    for U-NII-1, we should fall back to the unfiltered list rather
    than refuse to pick anything."""
    only_unii3 = [
        (channel_mod._CHANNEL_MAP[149], 12),
        (channel_mod._CHANNEL_MAP[157], 5),
    ]
    with mock.patch.object(
        channel_mod, "scan_channels", return_value=only_unii3
    ):
        ch = channel_mod.select_quietest_channel("wlan1", band="u-nii-1")
    # Fallback path: best of what was actually scanned
    assert ch.channel_number == 157


def test_select_quietest_channel_handles_dotted_band_spelling():
    """Config sometimes ships ``U-NII-1`` (uppercase, hyphenated) — the
    helper must tolerate either casing/spelling."""
    with mock.patch.object(
        channel_mod, "scan_channels", return_value=_build_scan_results()
    ):
        ch = channel_mod.select_quietest_channel("wlan1", band="U-NII-1")
    assert ch.channel_number == 36
