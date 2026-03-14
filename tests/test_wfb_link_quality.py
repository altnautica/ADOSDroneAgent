"""Tests for WFB-ng link quality monitoring and stats parsing."""

from __future__ import annotations

from ados.services.wfb.link_quality import LinkQualityMonitor, LinkStats, parse_wfb_rx_line

# --- parse_wfb_rx_line ---

def test_parse_standard_line():
    line = (
        "RX ANT 0: [aa:bb:cc:dd:ee:ff] "
        "rssi_min=-52 rssi_avg=-48 rssi_max=-44 "
        "packets=1234 lost=2 fec_rec=5 fec_fail=0"
    )
    stats = parse_wfb_rx_line(line)
    assert stats is not None
    assert stats.rssi_dbm == -48.0
    assert stats.rssi_min == -52.0
    assert stats.rssi_max == -44.0
    assert stats.packets_received == 1234
    assert stats.packets_lost == 2
    assert stats.fec_recovered == 5
    assert stats.fec_failed == 0
    assert stats.noise_dbm == -95.0  # default
    assert stats.snr_db == -48.0 - (-95.0)
    assert stats.timestamp != ""


def test_parse_line_with_noise():
    line = (
        "RX ANT 0: rssi_min=-60 rssi_avg=-55 rssi_max=-50 "
        "packets=500 lost=10 fec_rec=3 fec_fail=1 noise=-90"
    )
    stats = parse_wfb_rx_line(line)
    assert stats is not None
    assert stats.noise_dbm == -90.0
    assert stats.snr_db == -55.0 - (-90.0)


def test_parse_line_with_bitrate():
    line = (
        "rssi_min=-45 rssi_avg=-42 rssi_max=-40 "
        "packets=2000 lost=0 fec_rec=0 fec_fail=0 8000 kbit/s"
    )
    stats = parse_wfb_rx_line(line)
    assert stats is not None
    assert stats.bitrate_kbps == 8000


def test_parse_non_stats_line():
    assert parse_wfb_rx_line("Starting wfb_rx...") is None
    assert parse_wfb_rx_line("") is None
    assert parse_wfb_rx_line("Session started") is None


def test_parse_loss_percent():
    line = (
        "rssi_min=-70 rssi_avg=-65 rssi_max=-60 "
        "packets=900 lost=100 fec_rec=50 fec_fail=10"
    )
    stats = parse_wfb_rx_line(line)
    assert stats is not None
    assert stats.loss_percent == 10.0  # 100 / (900+100) * 100


def test_parse_zero_packets():
    line = (
        "rssi_min=-80 rssi_avg=-75 rssi_max=-70 "
        "packets=0 lost=0 fec_rec=0 fec_fail=0"
    )
    stats = parse_wfb_rx_line(line)
    assert stats is not None
    assert stats.loss_percent == 0.0


# --- LinkStats ---

def test_link_stats_to_dict():
    stats = LinkStats(rssi_dbm=-55.0, packets_received=100, loss_percent=1.5)
    d = stats.to_dict()
    assert d["rssi_dbm"] == -55.0
    assert d["packets_received"] == 100
    assert d["loss_percent"] == 1.5
    assert "timestamp" in d


# --- LinkQualityMonitor ---

def test_monitor_feed_valid_line():
    mon = LinkQualityMonitor()
    line = (
        "rssi_min=-52 rssi_avg=-48 rssi_max=-44 "
        "packets=1000 lost=5 fec_rec=2 fec_fail=0"
    )
    result = mon.feed_line(line)
    assert result is not None
    assert mon.sample_count == 1
    assert mon.get_current().rssi_dbm == -48.0


def test_monitor_feed_invalid_line():
    mon = LinkQualityMonitor()
    result = mon.feed_line("some random log message")
    assert result is None
    assert mon.sample_count == 0


def test_monitor_history():
    mon = LinkQualityMonitor()
    for i in range(5):
        line = (
            f"rssi_min=-{50+i} rssi_avg=-{48+i} rssi_max=-{46+i} "
            f"packets={1000+i} lost={i} fec_rec=0 fec_fail=0"
        )
        mon.feed_line(line)

    history = mon.get_history(seconds=60)
    assert len(history) == 5


def test_monitor_ring_buffer_limit():
    mon = LinkQualityMonitor(max_samples=3)
    for i in range(5):
        line = (
            f"rssi_min=-{50+i} rssi_avg=-{48+i} rssi_max=-{46+i} "
            f"packets={1000+i} lost=0 fec_rec=0 fec_fail=0"
        )
        mon.feed_line(line)

    assert mon.sample_count == 3
    # Should have the last 3 samples
    current = mon.get_current()
    assert current.rssi_dbm == -52.0  # -48 + 4 (last iteration i=4)


def test_monitor_clear():
    mon = LinkQualityMonitor()
    line = (
        "rssi_min=-52 rssi_avg=-48 rssi_max=-44 "
        "packets=1000 lost=5 fec_rec=2 fec_fail=0"
    )
    mon.feed_line(line)
    assert mon.sample_count == 1

    mon.clear()
    assert mon.sample_count == 0
    assert mon.get_current().rssi_dbm == -100.0


def test_monitor_get_history_empty():
    mon = LinkQualityMonitor()
    assert mon.get_history(seconds=60) == []
