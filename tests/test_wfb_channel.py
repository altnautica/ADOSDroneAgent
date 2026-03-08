"""Tests for WFB-ng channel management."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

from ados.services.wfb.channel import (
    STANDARD_CHANNELS,
    WfbChannel,
    _parse_scan_results,
    get_channel,
    scan_channels,
    select_best_channel,
    set_channel,
)


def test_standard_channels():
    assert len(STANDARD_CHANNELS) == 7
    nums = [ch.channel_number for ch in STANDARD_CHANNELS]
    assert 36 in nums
    assert 149 in nums
    assert 165 in nums


def test_get_channel_valid():
    ch = get_channel(149)
    assert ch is not None
    assert ch.frequency_mhz == 5745
    assert ch.channel_number == 149


def test_get_channel_invalid():
    assert get_channel(999) is None
    assert get_channel(1) is None


# --- scan results parsing ---

IW_SCAN_OUTPUT = """\
BSS aa:bb:cc:dd:ee:ff(on wlan0)
\tfreq: 5180
\tsignal: -65.00 dBm
BSS 11:22:33:44:55:66(on wlan0)
\tfreq: 5745
\tsignal: -72.00 dBm
BSS 77:88:99:aa:bb:cc(on wlan0)
\tfreq: 5745
\tsignal: -80.00 dBm
"""


def test_parse_scan_results():
    results = _parse_scan_results(IW_SCAN_OUTPUT)
    assert len(results) == 3
    assert results[0] == (5180, -65)
    assert results[1] == (5745, -72)
    assert results[2] == (5745, -80)


def test_parse_scan_results_empty():
    assert _parse_scan_results("") == []


# --- scan_channels ---

@patch("ados.services.wfb.channel.platform")
def test_scan_channels_non_linux(mock_platform):
    mock_platform.system.return_value = "Darwin"
    result = scan_channels("wlan0")
    assert len(result) == 7
    # All should have zero interference
    for _ch, count in result:
        assert count == 0


@patch("ados.services.wfb.channel.subprocess")
@patch("ados.services.wfb.channel.platform")
def test_scan_channels_with_networks(mock_platform, mock_subprocess):
    mock_platform.system.return_value = "Linux"
    mock_result = MagicMock()
    mock_result.returncode = 0
    mock_result.stdout = IW_SCAN_OUTPUT
    mock_subprocess.run.return_value = mock_result

    results = scan_channels("wlan0")
    assert len(results) == 7

    # Results should be sorted by interference (ascending)
    counts = [c for _ch, c in results]
    assert counts == sorted(counts)

    # Channel 149 (5745 MHz) should have 2 networks
    ch149_results = [(ch, c) for ch, c in results if ch.channel_number == 149]
    assert len(ch149_results) == 1
    assert ch149_results[0][1] == 2

    # Channel 36 (5180 MHz) should have 1 network
    ch36_results = [(ch, c) for ch, c in results if ch.channel_number == 36]
    assert len(ch36_results) == 1
    assert ch36_results[0][1] == 1


# --- select_best_channel ---

def test_select_best_channel_empty():
    ch = select_best_channel([])
    assert ch.channel_number == 149  # default


def test_select_best_channel_with_data():
    scan_data = [
        (WfbChannel(5785, 157), 0),
        (WfbChannel(5180, 36), 3),
        (WfbChannel(5745, 149), 5),
    ]
    ch = select_best_channel(scan_data)
    assert ch.channel_number == 157


# --- set_channel ---

@patch("ados.services.wfb.channel.platform")
def test_set_channel_non_linux(mock_platform):
    mock_platform.system.return_value = "Darwin"
    assert set_channel("wlan0", 149) is False


def test_set_channel_invalid():
    # On non-Linux, returns False before checking channel validity
    with patch("ados.services.wfb.channel.platform") as mock_platform:
        mock_platform.system.return_value = "Linux"
        assert set_channel("wlan0", 999) is False


@patch("ados.services.wfb.channel.subprocess")
@patch("ados.services.wfb.channel.platform")
def test_set_channel_success(mock_platform, mock_subprocess):
    mock_platform.system.return_value = "Linux"
    ok_result = MagicMock()
    ok_result.returncode = 0
    mock_subprocess.run.return_value = ok_result

    assert set_channel("wlan0", 149) is True


@patch("ados.services.wfb.channel.subprocess")
@patch("ados.services.wfb.channel.platform")
def test_set_channel_failure(mock_platform, mock_subprocess):
    mock_platform.system.return_value = "Linux"
    fail_result = MagicMock()
    fail_result.returncode = 1
    fail_result.stderr = "Device busy"
    mock_subprocess.run.return_value = fail_result

    assert set_channel("wlan0", 149) is False
