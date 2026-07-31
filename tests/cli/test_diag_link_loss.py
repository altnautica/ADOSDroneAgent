"""The link diagnostic must not call a lossy link healthy.

`link_diag` answers "can this radio decode the peer" — deaf, mis-keyed, jammed,
healthy. It is not a delivery verdict. A link that decodes cleanly while losing a
fifth of the stream answers "healthy" to that question and shows the operator a
frozen picture, which is precisely the reading that trains an operator to
distrust the diagnostic.
"""

from __future__ import annotations

from click.testing import CliRunner

from ados.cli.diag import diag_group, frame_delivery_percent


def _invoke(payload: dict, monkeypatch) -> str:
    monkeypatch.setattr("ados.cli.diag._request", lambda *a, **k: payload)
    result = CliRunner().invoke(diag_group, ["link"])
    assert result.exit_code == 0, result.output
    return result.output


def _healthy_but_lossy() -> dict:
    """The reading measured on a real ground station: every decode counter
    clean, a quarter of the video stream gone."""
    return {
        "link_diag": "healthy",
        "state": "active",
        "rssi_dbm": -36.0,
        "channel": 149,
        "packets_received": 485,
        "packets_all": 486,
        "decrypt_errors": 0,
        "packets_bad": 0,
        "packets_lost": 156,
        "fec_recovered": 18,
        "fec_failed": 25,
        "loss_percent": 24.29,
        "bitrate_mbps": 2.24,
    }


def _clean() -> dict:
    d = _healthy_but_lossy()
    d.update(packets_lost=0, fec_failed=0, loss_percent=0.0)
    return d


def test_a_lossy_link_is_not_reported_as_healthy(monkeypatch):
    out = _invoke(_healthy_but_lossy(), monkeypatch)
    assert "LOSSY" in out
    # The decode verdict is still shown, because it is true and useful — it is
    # just not the headline.
    assert "healthy" in out
    assert "24.3% packet loss" in out


def test_a_clean_link_still_reports_its_decode_verdict(monkeypatch):
    out = _invoke(_clean(), monkeypatch)
    assert "HEALTHY" in out
    assert "LOSSY" not in out


def test_the_loss_counters_are_on_screen(monkeypatch):
    # They were absent entirely, so nothing on screen contradicted HEALTHY.
    out = _invoke(_healthy_but_lossy(), monkeypatch)
    assert "Packets lost" in out
    assert "FEC failed" in out
    assert "Loss" in out


def test_frame_delivery_is_explained_when_lossy(monkeypatch):
    out = _invoke(_healthy_but_lossy(), monkeypatch)
    assert "of frames" in out
    assert "estimate" in out


def test_a_link_with_no_decode_reports_no_measurement_not_zero_loss(monkeypatch):
    deaf = _clean()
    deaf.update(packets_received=0, link_diag="deaf", loss_percent=0.0)
    out = _invoke(deaf, monkeypatch)
    assert "DEAF" in out
    assert "LOSSY" not in out, "0% loss on a deaf radio is absence of data, not a clean link"
    assert "no decode" in out


def test_frame_delivery_compounds_across_the_frame():
    # The whole point: packet loss and frame loss are very different numbers.
    assert frame_delivery_percent(0.0) == 100.0
    assert frame_delivery_percent(100.0) == 0.0
    # 5% packet loss is already a big frame loss at ~10 packets/frame.
    assert 55.0 < frame_delivery_percent(5.0) < 65.0
    # The measured 24% case delivered single-digit percentages of frames.
    assert frame_delivery_percent(24.29) < 10.0
    # Monotonic: more packet loss never delivers more frames.
    prev = 101.0
    for pct in range(0, 101, 5):
        cur = frame_delivery_percent(float(pct))
        assert cur <= prev
        prev = cur
