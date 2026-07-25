"""Tests for the ground-station setup-AP enable-marker reconcile + kick.

The regression these pin: the DHCP/DNS unit used to start whenever the
hostapd unit was active, but hostapd idles in place (staying `active`)
when the operator never opted the AP in — so dnsmasq started on a box
whose onboard radio was serving as a WiFi client, failed to bind an
address that already carried a lease, and left a permanently failed unit
on an otherwise healthy ground station.
"""

from __future__ import annotations

import ados.core.hotspot_marker as hotspot_marker
import ados.core.paths as paths
from ados.core.hotspot_marker import (
    _hotspot_slice,
    reconcile_hotspot_marker,
    sync_after_config_write,
)


def _enabled(value: bool, **fields) -> dict:
    return {"network": {"hotspot": {"enabled": value, **fields}}}


def test_reconcile_writes_and_removes_the_marker(monkeypatch, tmp_path) -> None:
    marker = tmp_path / "hotspot-enabled"
    monkeypatch.setattr(paths, "HOTSPOT_ENABLED_PATH", marker)

    assert reconcile_hotspot_marker(_enabled(True)) is True
    assert marker.exists()
    # Idempotent: same posture ⇒ no change.
    assert reconcile_hotspot_marker(_enabled(True)) is False
    assert marker.exists()
    # Disabling removes it, so the unit goes back to condition-skipped.
    assert reconcile_hotspot_marker(_enabled(False)) is True
    assert not marker.exists()
    assert reconcile_hotspot_marker(_enabled(False)) is False


def test_reconcile_treats_a_missing_block_as_opted_out(monkeypatch, tmp_path) -> None:
    marker = tmp_path / "hotspot-enabled"
    marker.touch()
    monkeypatch.setattr(paths, "HOTSPOT_ENABLED_PATH", marker)

    # No network/hotspot block anywhere ⇒ opted out. This is the default
    # posture on a fresh ground station, and it must leave NO marker: that
    # absence is what keeps dnsmasq cleanly skipped instead of failing.
    assert reconcile_hotspot_marker({"agent": {"name": "x"}}) is True
    assert not marker.exists()
    assert reconcile_hotspot_marker(None) is False


def test_hotspot_slice_is_total_over_malformed_shapes() -> None:
    assert _hotspot_slice(None) == {}
    assert _hotspot_slice({}) == {}
    assert _hotspot_slice({"network": "nope"}) == {}
    assert _hotspot_slice({"network": {"hotspot": ["nope"]}}) == {}
    assert _hotspot_slice(_enabled(True)) == {"enabled": True}


def test_sync_kicks_only_when_the_hotspot_slice_changes(monkeypatch, tmp_path) -> None:
    marker = tmp_path / "hotspot-enabled"
    monkeypatch.setattr(paths, "HOTSPOT_ENABLED_PATH", marker)
    kicks: list[tuple[str, str]] = []
    monkeypatch.setattr(
        hotspot_marker, "_kick", lambda unit, verb: kicks.append((unit, verb))
    )

    # An unrelated config change (same hotspot slice) never churns the AP.
    sync_after_config_write(_enabled(False), {**_enabled(False), "agent": {"n": 1}})
    assert kicks == []

    # Opting in flips the marker and kicks both units.
    sync_after_config_write(_enabled(False), _enabled(True))
    assert marker.exists()
    assert ("ados-dnsmasq-gs.service", "reload-or-restart") in kicks
    # hostapd is try-restart so a profile keeping it stopped is never
    # force-started by a config save.
    assert ("ados-hostapd.service", "try-restart") in kicks


def test_opting_out_clears_a_latched_dnsmasq_failure(monkeypatch, tmp_path) -> None:
    """Turning the AP off must reset the unit's remembered failure.

    systemd keeps a unit's last failed state until it is reset, so without
    this an operator who opts the AP back out still sees a failed unit
    even though the condition now skips it cleanly.
    """
    marker = tmp_path / "hotspot-enabled"
    marker.touch()
    monkeypatch.setattr(paths, "HOTSPOT_ENABLED_PATH", marker)
    kicks: list[tuple[str, str]] = []
    monkeypatch.setattr(
        hotspot_marker, "_kick", lambda unit, verb: kicks.append((unit, verb))
    )

    sync_after_config_write(_enabled(True), _enabled(False))
    assert not marker.exists()
    assert ("ados-dnsmasq-gs.service", "reset-failed") in kicks
    assert ("ados-dnsmasq-gs.service", "stop") in kicks
    # Never a restart on the way out — that would re-run the failing start.
    assert ("ados-dnsmasq-gs.service", "reload-or-restart") not in kicks


def test_sync_kicks_on_a_same_posture_slice_edit(monkeypatch, tmp_path) -> None:
    """A channel/password edit with enabled unchanged still restarts the AP."""
    marker = tmp_path / "hotspot-enabled"
    marker.touch()
    monkeypatch.setattr(paths, "HOTSPOT_ENABLED_PATH", marker)
    kicks: list[tuple[str, str]] = []
    monkeypatch.setattr(
        hotspot_marker, "_kick", lambda unit, verb: kicks.append((unit, verb))
    )

    sync_after_config_write(_enabled(True, channel=1), _enabled(True, channel=6))
    assert kicks  # slice changed even though the marker did not
    assert marker.exists()


def test_sync_never_raises_when_the_marker_path_is_unwritable(
    monkeypatch, tmp_path
) -> None:
    """A marker hiccup must never fail the config write that already landed."""
    # Point at a path whose parent does not exist so touch() raises OSError.
    monkeypatch.setattr(
        paths, "HOTSPOT_ENABLED_PATH", tmp_path / "missing" / "hotspot-enabled"
    )
    monkeypatch.setattr(hotspot_marker, "_kick", lambda unit, verb: None)
    assert reconcile_hotspot_marker(_enabled(True)) is False
    sync_after_config_write(_enabled(False), _enabled(True))
