"""Tests for the CRSF RC-lane enable-marker reconcile + service kick."""

from __future__ import annotations

import ados.core.crsf_marker as crsf_marker
import ados.core.paths as paths
from ados.core.crsf_marker import (
    _crsf_slice,
    reconcile_crsf_marker,
    sync_after_config_write,
)


def _enabled(value: bool) -> dict:
    return {"radio": {"crsf": {"enabled": value, "device": None}}}


def test_reconcile_writes_and_removes_the_marker(monkeypatch, tmp_path) -> None:
    marker = tmp_path / "crsf-enabled"
    monkeypatch.setattr(paths, "CRSF_ENABLED_PATH", marker)

    # Enabling writes the marker and reports the flip.
    assert reconcile_crsf_marker(_enabled(True)) is True
    assert marker.exists()
    # Idempotent: a second reconcile with the same posture is a no-change.
    assert reconcile_crsf_marker(_enabled(True)) is False
    assert marker.exists()
    # Disabling removes it.
    assert reconcile_crsf_marker(_enabled(False)) is True
    assert not marker.exists()
    assert reconcile_crsf_marker(_enabled(False)) is False


def test_reconcile_treats_a_missing_block_as_opted_out(monkeypatch, tmp_path) -> None:
    marker = tmp_path / "crsf-enabled"
    marker.touch()
    monkeypatch.setattr(paths, "CRSF_ENABLED_PATH", marker)

    # No radio/crsf block anywhere ⇒ the lane is opted out; a lingering
    # marker is cleaned up rather than left asserting an enable.
    assert reconcile_crsf_marker({"agent": {"name": "x"}}) is True
    assert not marker.exists()
    assert reconcile_crsf_marker(None) is False


def test_crsf_slice_is_total_over_malformed_shapes() -> None:
    assert _crsf_slice(None) == {}
    assert _crsf_slice({}) == {}
    assert _crsf_slice({"radio": "nope"}) == {}
    assert _crsf_slice({"radio": {"crsf": ["nope"]}}) == {}
    assert _crsf_slice(_enabled(True)) == {"enabled": True, "device": None}


def test_sync_kicks_only_when_the_lane_slice_changes(monkeypatch, tmp_path) -> None:
    marker = tmp_path / "crsf-enabled"
    monkeypatch.setattr(paths, "CRSF_ENABLED_PATH", marker)
    kicks: list[bool] = []
    monkeypatch.setattr(crsf_marker, "_kick_crsf_service", lambda: kicks.append(True))

    # An unrelated config change (same lane slice) never churns the unit.
    prev = _enabled(False)
    same = {"radio": {"crsf": {"enabled": False, "device": None}}, "agent": {"name": "y"}}
    sync_after_config_write(prev, same)
    assert kicks == []

    # Enabling the lane flips the marker AND kicks the unit.
    sync_after_config_write(prev, _enabled(True))
    assert marker.exists()
    assert len(kicks) == 1

    # A same-enable device re-pin (marker unchanged, slice changed) still
    # kicks, so the running lane reloads onto the new port.
    repinned = {"radio": {"crsf": {"enabled": True, "device": "/dev/ttyUSB1"}}}
    sync_after_config_write(_enabled(True), repinned)
    assert len(kicks) == 2
