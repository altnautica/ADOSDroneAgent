"""Tests for `ados diag storage` rendering.

The point of this surface is that an operator can tell "healthy" from "nobody
measured" at a glance. Four SD cards were reflashed while every reading was
either absent or unread, so the cases that matter most here are the ones where a
number does NOT exist: they must render as a stated absence, never as a zero that
reads like a clean bill of health.
"""

from __future__ import annotations

from unittest.mock import patch

from click.testing import CliRunner

from ados.cli.diag import diag_group


def _invoke(payload: dict) -> str:
    runner = CliRunner()
    with patch("ados.cli.diag._request", return_value=payload):
        result = runner.invoke(diag_group, ["storage"])
    assert result.exit_code == 0, result.output
    return result.output


def _payload(**overrides) -> dict:
    base = {
        "verdict": "ok",
        "reason": "writing 1 KB/s sustained; no throttle events recorded",
        "write": {
            "kb_per_s": 1.0,
            "gb_per_day": 0.1,
            "window_s": 120.0,
            "device": "mmcblk0",
            "reason": None,
        },
        "throttle": {"supported": True, "clean": True},
        "store": {"live_bytes": 1048576, "wal_bytes": 4096, "quarantined": 0},
        "filesystem": {"total_bytes": 32212254720, "used_bytes": 6442450944,
                       "used_pct": 20.0, "reason": None},
    }
    base.update(overrides)
    return base


def test_healthy_box_reports_the_rate_and_the_device():
    out = _invoke(_payload())
    assert "OK" in out
    assert "1.0 KB/s" in out
    assert "mmcblk0" in out
    assert "120s window" in out


def test_an_unmeasured_rate_states_why_and_never_prints_a_zero():
    out = _invoke(
        _payload(
            verdict="unknown",
            reason="the logging store did not answer",
            write={"kb_per_s": None, "gb_per_day": None, "window_s": None,
                   "device": None, "reason": "the logging store did not answer"},
        )
    )
    assert "UNKNOWN" in out
    assert "the logging store did not answer" in out
    assert "0.0 KB/s" not in out


def test_a_recorded_undervoltage_is_named_not_summarised_as_a_flag():
    out = _invoke(
        _payload(
            verdict="wearing",
            throttle={
                "supported": True,
                "clean": False,
                "undervoltage_occurred": True,
                "arm_frequency_capped_occurred": False,
                "throttling_occurred": False,
                "soft_temperature_limit_occurred": False,
            },
        )
    )
    assert "undervoltage" in out
    assert "retained window" in out


def test_a_board_with_no_throttle_bitfield_does_not_read_as_clean():
    out = _invoke(
        _payload(
            throttle={
                "supported": False,
                "reason": "this board does not report a throttle bitfield",
            }
        )
    )
    assert "does not report a throttle bitfield" in out
    assert "clean, no events recorded" not in out


def test_quarantined_corpses_are_surfaced_with_their_size():
    out = _invoke(
        _payload(
            store={
                "live_bytes": 935616512,
                "wal_bytes": 4387832,
                "quarantined": 2,
                "quarantined_bytes": 1620725760,
            }
        )
    )
    assert "2 torn store(s)" in out
    assert "not reclaimed" in out


def test_a_critical_verdict_carries_its_reason():
    out = _invoke(
        _payload(
            verdict="critical",
            reason="writing 1714 KB/s sustained (141 GB/day); this is the rate "
            "that wore a card out in under two days",
            write={"kb_per_s": 1714.0, "gb_per_day": 141.0, "window_s": 120.0,
                   "device": "mmcblk0", "reason": None},
        )
    )
    assert "CRITICAL" in out
    assert "wore a card out" in out
    assert "1714.0 KB/s" in out
