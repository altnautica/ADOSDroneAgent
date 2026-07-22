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
    monkeypatch.setattr(crsf_marker, "_kick_mavlink_router", lambda: None)

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


def _lane(**fields) -> dict:
    return {"radio": {"crsf": {"enabled": True, "device": "/dev/ttyUSB0", **fields}}}


def test_sync_kicks_the_router_only_when_its_view_changes(monkeypatch, tmp_path) -> None:
    """The router consumes only the pin + the resolved MAVLink-over-ELRS
    source; lane-only knobs must never restart the FC link."""
    marker = tmp_path / "crsf-enabled"
    monkeypatch.setattr(paths, "CRSF_ENABLED_PATH", marker)
    lane_kicks: list[bool] = []
    router_kicks: list[bool] = []
    monkeypatch.setattr(crsf_marker, "_kick_crsf_service", lambda: lane_kicks.append(True))
    monkeypatch.setattr(crsf_marker, "_kick_mavlink_router", lambda: router_kicks.append(True))

    # A lane-only knob (packet rate): the lane reloads, the router does not.
    sync_after_config_write(_lane(packet_rate_hz=150), _lane(packet_rate_hz=250))
    assert len(lane_kicks) == 1
    assert router_kicks == []

    # Flipping the mode to mavlink resolves the router's ingest source.
    sync_after_config_write(_lane(), _lane(mode="mavlink"))
    assert len(router_kicks) == 1

    # Changing the carrier while in mavlink mode changes the router's view.
    sync_after_config_write(
        _lane(mode="mavlink"), _lane(mode="mavlink", mavlink_transport="backpack_wifi")
    )
    assert len(router_kicks) == 2

    # The same carrier flip while in crsf_rc mode resolves no source either
    # way: no router churn.
    sync_after_config_write(_lane(), _lane(mavlink_transport="backpack_wifi"))
    assert len(router_kicks) == 2

    # A device re-pin always reaches the router (the FC-candidacy exclusion
    # and the serial ingest both key on it).
    sync_after_config_write(_lane(), {"radio": {"crsf": {"enabled": True, "device": "/dev/ttyUSB1"}}})
    assert len(router_kicks) == 3

    # An enable flip with the mode at its crsf_rc default keeps the router's
    # view identical (same pin, no source): lane kick only.
    before = len(lane_kicks)
    sync_after_config_write(_lane(), _lane(enabled=False))
    assert len(lane_kicks) == before + 1
    assert len(router_kicks) == 3

    # Disabling the lane while in mavlink mode drops the resolved source.
    sync_after_config_write(_lane(mode="mavlink"), _lane(mode="mavlink", enabled=False))
    assert len(router_kicks) == 4


def test_router_view_projection() -> None:
    """The pure projection: (pin, resolved source)."""
    view = crsf_marker._router_view
    assert view({}) == (None, None)
    assert view({"enabled": True, "device": "/dev/ttyUSB0"}) == ("/dev/ttyUSB0", None)
    assert view({"enabled": True, "device": "/dev/ttyUSB0", "mode": "mavlink"}) == (
        "/dev/ttyUSB0",
        "serial",
    )
    # An unknown transport degrades to the serial carrier, matching the
    # router's own parse of the block.
    assert view(
        {"enabled": True, "device": None, "mode": "mavlink", "mavlink_transport": "bogus"}
    ) == (None, "serial")
    assert view(
        {"enabled": True, "device": None, "mode": "mavlink", "mavlink_transport": "backpack_wifi"}
    ) == (None, "backpack_wifi")
    # Opted out, or any other mode: no resolved source.
    assert view({"enabled": False, "mode": "mavlink"}) == (None, None)
    assert view({"enabled": True, "mode": "airport", "device": "/dev/ttyUSB0"}) == (
        "/dev/ttyUSB0",
        None,
    )
