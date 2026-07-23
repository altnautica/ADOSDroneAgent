"""Tests for the config-over-radio enable-marker reconcile + service kick."""

from __future__ import annotations

import ados.core.paths as paths
import ados.core.tunnel_marker as tunnel_marker
from ados.core.tunnel_marker import (
    _tunnel_slice,
    reconcile_tunnel_marker,
    sync_after_config_write,
)


def _enabled(value: bool, **fields) -> dict:
    return {"radio": {"tunnel": {"enabled": value, **fields}}}


def test_reconcile_writes_and_removes_the_marker(monkeypatch, tmp_path) -> None:
    marker = tmp_path / "tunnel-enabled"
    monkeypatch.setattr(paths, "TUNNEL_ENABLED_PATH", marker)

    assert reconcile_tunnel_marker(_enabled(True)) is True
    assert marker.exists()
    # Idempotent: same posture ⇒ no change.
    assert reconcile_tunnel_marker(_enabled(True)) is False
    assert marker.exists()
    # Disabling removes it.
    assert reconcile_tunnel_marker(_enabled(False)) is True
    assert not marker.exists()
    assert reconcile_tunnel_marker(_enabled(False)) is False


def test_reconcile_treats_a_missing_block_as_opted_out(monkeypatch, tmp_path) -> None:
    marker = tmp_path / "tunnel-enabled"
    marker.touch()
    monkeypatch.setattr(paths, "TUNNEL_ENABLED_PATH", marker)

    # No radio/tunnel block anywhere ⇒ opted out; a lingering marker is cleaned
    # up rather than left asserting an enable.
    assert reconcile_tunnel_marker({"agent": {"name": "x"}}) is True
    assert not marker.exists()
    assert reconcile_tunnel_marker(None) is False


def test_tunnel_slice_is_total_over_malformed_shapes() -> None:
    assert _tunnel_slice(None) == {}
    assert _tunnel_slice({}) == {}
    assert _tunnel_slice({"radio": "nope"}) == {}
    assert _tunnel_slice({"radio": {"tunnel": ["nope"]}}) == {}
    assert _tunnel_slice(_enabled(True)) == {"enabled": True}


def test_sync_kicks_only_when_the_channel_slice_changes(monkeypatch, tmp_path) -> None:
    marker = tmp_path / "tunnel-enabled"
    monkeypatch.setattr(paths, "TUNNEL_ENABLED_PATH", marker)
    kicks: list[bool] = []
    monkeypatch.setattr(tunnel_marker, "_kick_tunnel_service", lambda: kicks.append(True))

    # An unrelated config change (same channel slice) never churns the unit.
    prev = _enabled(False)
    same = {"radio": {"tunnel": {"enabled": False}}, "agent": {"name": "y"}}
    sync_after_config_write(prev, same)
    assert kicks == []

    # Enabling the channel flips the marker AND kicks the unit.
    sync_after_config_write(prev, _enabled(True))
    assert marker.exists()
    assert len(kicks) == 1

    # Opening the write gate (marker unchanged, slice changed) still kicks, so
    # the running service reloads onto the fresh command_enabled posture.
    sync_after_config_write(_enabled(True), _enabled(True, command_enabled=True))
    assert marker.exists()
    assert len(kicks) == 2

    # Disabling flips the marker off and kicks a final time.
    sync_after_config_write(_enabled(True, command_enabled=True), _enabled(False))
    assert not marker.exists()
    assert len(kicks) == 3
