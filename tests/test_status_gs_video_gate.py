"""Ground-station video state must reflect a live downlink, not WHEP reach.

mediamtx on the ground station serves a WHEP endpoint whether or not any
frames are arriving over the radio. Reporting "running" purely because
the WHEP probe is reachable shows "Video: Live" over a dead radio. The
consolidated status gates the ground-station video state on the WFB
receive link actually delivering video: link connected AND a positive
valid-decode rate. (Operating rule 37: endpoint-reachable is never proof
of data flowing.)
"""

from __future__ import annotations

from ados.api.routes.status import _gs_video_delivering


def test_not_delivering_when_no_wfb_status():
    assert _gs_video_delivering(None) is False
    assert _gs_video_delivering({}) is False


def test_not_delivering_when_link_not_connected():
    # Endpoint may be reachable, but the link never decoded packets.
    status = {"state": "connecting", "valid_rx_packets_per_s": 0, "packets_received": 0}
    assert _gs_video_delivering(status) is False


def test_not_delivering_when_connected_but_zero_valid_rx():
    # Paired-but-silent: connected state but no frames flowing right now.
    status = {"state": "connected", "valid_rx_packets_per_s": 0, "packets_received": 0}
    assert _gs_video_delivering(status) is False


def test_delivering_when_connected_and_valid_rx_positive():
    status = {"state": "connected", "valid_rx_packets_per_s": 42.0, "packets_received": 0}
    assert _gs_video_delivering(status) is True


def test_delivering_when_connected_and_packets_received_positive():
    # Falls back to the cumulative packet count when the per-second rate
    # is absent (older stats files).
    status = {"state": "connected", "packets_received": 100}
    assert _gs_video_delivering(status) is True


def test_stale_link_is_not_delivering():
    # A stale stats file marks state="stale"; not a live state → not live.
    status = {"state": "stale", "valid_rx_packets_per_s": 5.0, "packets_received": 50}
    assert _gs_video_delivering(status) is False


def test_delivering_when_active_and_valid_rx_positive():
    # The ground-link stats writer marks the live receiving state as "active"
    # (it does not emit "connected"); a live "active" link with a positive
    # decode rate must report delivering.
    status = {"state": "active", "valid_rx_packets_per_s": 656.0, "packets_received": 656}
    assert _gs_video_delivering(status) is True


def test_active_but_silent_is_not_delivering():
    # "active" but no frames flowing right now is not delivering (rule 37).
    status = {"state": "active", "valid_rx_packets_per_s": 0, "packets_received": 0}
    assert _gs_video_delivering(status) is False
