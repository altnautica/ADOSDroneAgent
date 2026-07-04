"""Tests for the upstream-format wfb-ng v26.4 stats parser.

The samples below are real strings captured live via ``strace`` on a
running ``wfb_rx -l 1000 ... wlan1`` on a ground-station rig. The
upstream emit format is:

    IPC_MSG("%llu\\tRX_ANT\\t%u:%u:%u\\t%llx\\t%d:%d:%d:%d:%d:%d:%d\\n", ...);
    IPC_MSG("%llu\\tPKT\\t%u:%u:%u:%u:%u:%u:%u:%u:%u:%u:%u\\n", ...);
"""

from __future__ import annotations

from ados.services.wfb.link_quality import (
    LinkQualityMonitor,
    LinkStats,
    parse_pkt_line,
    parse_rx_ant_line,
    parse_wfb_rx_line,
)

# --- single-line parsers ---


def test_parse_rx_ant_line() -> None:
    """Real captured output: ts=1175372 ms, freq=5745 MHz, mcs=1, bw=20,
    ant_id=1, count=649, rssi (-50,-48,-45), snr (35,38,42)."""
    line = "1175372\tRX_ANT\t5745:1:20\t1\t649:-50:-48:-45:35:38:42"
    rx = parse_rx_ant_line(line)
    assert rx is not None
    assert rx.freq_mhz == 5745
    assert rx.mcs_index == 1
    assert rx.bandwidth_mhz == 20
    assert rx.antenna_id == 1
    assert rx.count == 649
    assert rx.rssi_min == -50
    assert rx.rssi_avg == -48
    assert rx.rssi_max == -45
    assert rx.snr_min == 35
    assert rx.snr_avg == 38
    assert rx.snr_max == 42


def test_parse_pkt_line() -> None:
    """Realistic PKT shape: 700 received, 700 unique data, 4 fec
    recovered, 0 lost, 8 Mbps outgoing → 1 MB/s = 1_000_000 b_outgoing."""
    line = "1175372\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000"
    pkt = parse_pkt_line(line)
    assert pkt is not None
    assert pkt.count_p_all == 720
    assert pkt.count_b_all == 1075000
    assert pkt.count_p_dec_err == 0
    assert pkt.count_p_session == 5
    assert pkt.count_p_data == 715
    assert pkt.count_p_uniq == 715
    assert pkt.count_p_fec_recovered == 4
    assert pkt.count_p_lost == 0
    assert pkt.count_p_bad == 0
    assert pkt.count_p_outgoing == 715
    assert pkt.count_b_outgoing == 1000000


def test_parse_rx_ant_returns_none_on_pkt_line() -> None:
    """Cross-shape inputs must not match each other's regex."""
    line = "1175372\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000"
    assert parse_rx_ant_line(line) is None


def test_parse_pkt_returns_none_on_rx_ant_line() -> None:
    line = "1175372\tRX_ANT\t5745:1:20\t1\t649:-50:-48:-45:35:38:42"
    assert parse_pkt_line(line) is None


def test_parse_garbage_returns_none() -> None:
    assert parse_rx_ant_line("Starting wfb_rx...") is None
    assert parse_pkt_line("Starting wfb_rx...") is None
    assert parse_rx_ant_line("") is None
    assert parse_pkt_line("") is None
    assert parse_rx_ant_line("1175372\tBOGUS\tfields") is None


def test_parse_rx_ant_handles_multi_digit_antenna_hex() -> None:
    line = "9999999\tRX_ANT\t5180:7:40\tff\t100:-70:-65:-60:10:15:20"
    rx = parse_rx_ant_line(line)
    assert rx is not None
    assert rx.antenna_id == 0xFF
    assert rx.freq_mhz == 5180
    assert rx.bandwidth_mhz == 40
    assert rx.mcs_index == 7


# --- legacy single-line wrapper ---


def test_legacy_parse_wfb_rx_line_extracts_rssi_only_from_rx_ant() -> None:
    """The legacy entrypoint returns a partial LinkStats from RX_ANT
    alone (with packet counters at zero) so callers that pre-date
    the stateful aggregator keep working."""
    line = "1175372\tRX_ANT\t5745:1:20\t1\t649:-50:-48:-45:35:38:42"
    stats = parse_wfb_rx_line(line)
    assert stats is not None
    assert stats.rssi_dbm == -48.0
    assert stats.snr_db == 38.0
    # Packet counters not present in an RX_ANT line — defaults.
    assert stats.packets_received == 0


def test_legacy_parse_wfb_rx_line_extracts_packets_from_pkt() -> None:
    line = "1175372\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000"
    stats = parse_wfb_rx_line(line)
    assert stats is not None
    assert stats.packets_received == 715
    assert stats.fec_recovered == 4


# --- LinkQualityMonitor (stateful) ---


def test_monitor_emits_on_rx_ant_and_pkt_lines() -> None:
    """Both line types emit a snapshot so RSSI/SNR (and the stats-file
    write that rides on a non-None return) never depend on the matching
    PKT line for the interval also parsing.

    An RX_ANT line before any PKT emits a snapshot with fresh RSSI but
    zero packet counters; the PKT line that follows emits one with the
    counters filled in.
    """
    mon = LinkQualityMonitor()
    rx_line = "1175372\tRX_ANT\t5745:1:20\t1\t649:-50:-48:-45:35:38:42"
    pkt_line = "1175372\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000"

    rx_snap = mon.feed_line(rx_line)
    assert rx_snap is not None
    assert rx_snap.rssi_dbm == -48.0
    # No PKT seen yet → counters read zero on the RX_ANT-only emit.
    assert rx_snap.packets_received == 0
    assert mon.sample_count == 1

    snap = mon.feed_line(pkt_line)
    assert snap is not None
    assert snap.packets_received == 715
    assert mon.sample_count == 2


def test_monitor_combines_rx_ant_and_pkt_into_one_snapshot() -> None:
    mon = LinkQualityMonitor()
    mon.feed_line("1175372\tRX_ANT\t5745:1:20\t1\t649:-50:-48:-45:35:38:42")
    snap = mon.feed_line(
        "1175372\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000"
    )
    assert snap is not None
    # RSSI from RX_ANT
    assert snap.rssi_dbm == -48.0
    assert snap.snr_db == 38.0
    # Counters from PKT
    assert snap.packets_received == 715
    assert snap.fec_recovered == 4
    # Bitrate derived from b_outgoing / interval (default 1.0 s).
    # 1_000_000 bytes/s * 8 = 8_000_000 bps = 8000 kbps.
    assert snap.bitrate_kbps == 8000


def test_monitor_loss_percent_only_uses_data_and_lost() -> None:
    mon = LinkQualityMonitor()
    mon.feed_line(
        "1175372\tRX_ANT\t5745:1:20\t1\t800:-60:-58:-55:20:25:30"
    )
    snap = mon.feed_line(
        # 700 data, 50 lost, dec_err 100 (irrelevant — keys mismatched)
        "1175372\tPKT\t850:1000000:100:5:700:700:0:50:0:700:1000000"
    )
    assert snap is not None
    # 50 / (700 + 50) = 6.67%
    assert snap.loss_percent == 6.67


def test_monitor_handles_missing_rx_ant_gracefully() -> None:
    """If a PKT line arrives with no preceding RX_ANT (out-of-order
    boot, clock skew), the snapshot still produces with default RSSI."""
    mon = LinkQualityMonitor()
    snap = mon.feed_line(
        "1175372\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000"
    )
    assert snap is not None
    assert snap.rssi_dbm == -100.0  # default
    assert snap.packets_received == 715  # PKT counters still populated


def test_monitor_history_grows_with_stats_lines() -> None:
    mon = LinkQualityMonitor(max_samples=3)
    for i in range(5):
        mon.feed_line(
            f"{1000+i}\tRX_ANT\t5745:1:20\t1\t100:-50:-48:-45:35:38:42"
        )
        mon.feed_line(
            f"{1000+i}\tPKT\t100:100000:0:5:95:95:0:0:0:95:90000"
        )
    # Both RX_ANT and PKT lines append a sample now; the ring buffer is
    # still bounded to its max regardless of which line type fills it.
    assert mon.sample_count == 3
    history = mon.get_history(seconds=60)
    assert len(history) == 3


def test_monitor_clear_resets_aggregator_state() -> None:
    mon = LinkQualityMonitor()
    mon.feed_line(
        "1175372\tRX_ANT\t5745:1:20\t1\t649:-50:-48:-45:35:38:42"
    )
    mon.feed_line(
        "1175372\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000"
    )
    # RX_ANT and PKT each append a sample now.
    assert mon.sample_count == 2

    mon.clear()
    assert mon.sample_count == 0
    assert mon.get_current().rssi_dbm == -100.0
    # After clear, a lone PKT must still work but with default RSSI
    snap = mon.feed_line(
        "1175372\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000"
    )
    assert snap is not None
    assert snap.rssi_dbm == -100.0


def test_monitor_get_history_empty() -> None:
    mon = LinkQualityMonitor()
    assert mon.get_history(seconds=60) == []


def test_monitor_garbage_lines_ignored() -> None:
    mon = LinkQualityMonitor()
    assert mon.feed_line("some random log line") is None
    assert mon.feed_line("Starting wfb_rx...") is None
    assert mon.sample_count == 0


# --- persist_to_file ---


def test_monitor_persist_to_file_writes_atomic_json(tmp_path) -> None:
    import json

    mon = LinkQualityMonitor()
    mon.feed_line(
        "1175372\tRX_ANT\t5745:1:20\t1\t649:-50:-48:-45:35:38:42"
    )
    mon.feed_line(
        "1175372\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000"
    )
    out = tmp_path / "wfb-stats.json"
    mon.persist_to_file(
        out, extra={"channel": 149, "interface": "wlan1", "profile": "ground_station"},
    )
    assert out.exists()
    payload = json.loads(out.read_text())
    assert payload["rssi_dbm"] == -48.0
    assert payload["packets_received"] == 715
    assert payload["bitrate_kbps"] == 8000
    assert payload["channel"] == 149
    assert payload["state"] == "connected"


def test_monitor_persist_to_file_state_connecting_when_no_packets(
    tmp_path,
) -> None:
    """A fresh monitor with no PKT line yet shows state=connecting."""
    import json

    mon = LinkQualityMonitor()
    out = tmp_path / "wfb-stats.json"
    mon.persist_to_file(out)
    payload = json.loads(out.read_text())
    assert payload["state"] == "connecting"
    assert payload["packets_received"] == 0


# --- LinkStats compatibility ---


def test_link_stats_to_dict_round_trip() -> None:
    stats = LinkStats(rssi_dbm=-55.0, packets_received=100, loss_percent=1.5)
    d = stats.to_dict()
    assert d["rssi_dbm"] == -55.0
    assert d["packets_received"] == 100
    assert d["loss_percent"] == 1.5
    assert "timestamp" in d


# --- format-drift tolerance (on-rig observability regression) ---


def test_pkt_line_tolerates_extra_trailing_fields() -> None:
    """A receiver build that appends a 12th counter must still parse.

    A $-anchored 11-field pattern dropped these lines entirely, so
    packets_received / bitrate read zero while video decoded fine and the
    stats file never got written.
    """
    line = (
        "1175372\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000:9"
    )
    pkt = parse_pkt_line(line)
    assert pkt is not None
    # Leading fields we consume are still correct; the extra trailing
    # counter is ignored.
    assert pkt.count_p_data == 715
    assert pkt.count_b_outgoing == 1000000


def test_rx_ant_line_tolerates_extra_trailing_fields() -> None:
    line = "1175372\tRX_ANT\t5745:1:20\t1\t649:-50:-48:-45:35:38:42:7"
    rx = parse_rx_ant_line(line)
    assert rx is not None
    assert rx.rssi_avg == -48
    assert rx.snr_avg == 38


def test_packets_bitrate_populated_from_extra_field_pkt_line() -> None:
    """End-to-end: the 12-field PKT line populates packets + bitrate."""
    mon = LinkQualityMonitor()
    mon.feed_line("1175372\tRX_ANT\t5745:1:20\t1\t649:-50:-48:-45:35:38:42")
    snap = mon.feed_line(
        "1175372\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000:9"
    )
    assert snap is not None
    assert snap.packets_received == 715
    assert snap.bitrate_kbps == 8000
    assert snap.rssi_dbm == -48.0


def test_rx_ant_emit_carries_forward_last_pkt_counters() -> None:
    """An RX_ANT-only interval keeps packets/bitrate from the last PKT.

    On a rig whose PKT line for an interval fails to parse, the RX_ANT
    line still emits a snapshot carrying the most recent packet/bitrate
    counters so RSSI stays fresh AND the consumer (stats-file writer)
    keeps seeing live data.
    """
    mon = LinkQualityMonitor()
    mon.feed_line("1\tRX_ANT\t5745:1:20\t1\t100:-50:-48:-45:35:38:42")
    mon.feed_line("1\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000")
    # A later RX_ANT line with no following PKT still reports the
    # carried-forward counters.
    snap = mon.feed_line("2\tRX_ANT\t5745:1:20\t1\t100:-49:-47:-44:36:39:43")
    assert snap is not None
    assert snap.rssi_dbm == -47.0  # fresh from the new RX_ANT
    assert snap.packets_received == 715  # carried forward
    assert snap.bitrate_kbps == 8000  # carried forward


def test_persist_to_file_populated_after_pkt(tmp_path) -> None:
    """The persisted snapshot carries packets + bitrate after a PKT line."""
    import json

    mon = LinkQualityMonitor()
    mon.feed_line("1\tRX_ANT\t5745:1:20\t1\t100:-50:-48:-45:35:38:42")
    mon.feed_line("1\tPKT\t720:1075000:0:5:715:715:4:0:0:715:1000000")
    out = tmp_path / "wfb-stats.json"
    mon.persist_to_file(out, extra={"profile": "ground_station"})
    payload = json.loads(out.read_text())
    assert payload["packets_received"] == 715
    assert payload["bitrate_kbps"] == 8000
    assert payload["state"] == "connected"
