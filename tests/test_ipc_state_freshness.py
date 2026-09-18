"""The MAVLink state-IPC view must not serve a snapshot the router stopped refreshing.

The native router owns the FC link and publishes the vehicle snapshot from
another process at ~10 Hz. The Python readers held the last snapshot with no age
bound and could not be cleared by any publication, so a router that died left
`fc_connected` / `armed` / battery / GPS frozen at their last values and served
them as current readings to `/api/telemetry`, the MQTT gateway, and the photo
geotagger.
"""

from __future__ import annotations

from ados.services.mavlink.ipc_state import (
    POSITION_FRESH_MAX_AGE_MS,
    STATE_SNAPSHOT_MAX_AGE_S,
    IpcFcConnection,
    IpcVehicleState,
)


class _Clock:
    """Hand-cranked monotonic clock, so the age gate is tested without sleeping."""

    def __init__(self) -> None:
        self.now = 1000.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


def _live_snapshot() -> dict:
    """An armed vehicle on a healthy FC link, as the router publishes it."""
    return {
        "armed": True,
        "mode": "GUIDED",
        "fc_connected": True,
        "transport_open": True,
        "mavlink_alive": True,
        "battery": {"voltage": 24.1, "remaining": 61},
        "gps": {"fix_type": 3, "satellites": 14},
    }


def test_a_snapshot_the_router_stopped_refreshing_stops_being_served() -> None:
    clock = _Clock()
    vs = IpcVehicleState(clock=clock)
    fc = IpcFcConnection(vs)
    vs.update_from_dict(_live_snapshot())

    assert vs.armed is True
    assert fc.connected is True
    assert vs.to_dict()["battery"]["remaining"] == 61

    clock.advance(STATE_SNAPSHOT_MAX_AGE_S + 0.5)

    # The exact failure: an armed vehicle with a live FC link and 61% battery,
    # reported off a router that has published nothing for seconds.
    assert vs.stale is True
    assert vs.age_s == STATE_SNAPSHOT_MAX_AGE_S + 0.5
    assert vs.armed is False
    assert vs.mode == ""
    assert fc.connected is False
    assert fc.mavlink_alive is False
    assert vs.to_dict()["battery"]["remaining"] == -1
    assert vs.to_dict()["gps"]["fix_type"] == 0


def test_a_fresh_publication_restores_the_reading() -> None:
    clock = _Clock()
    vs = IpcVehicleState(clock=clock)
    vs.update_from_dict(_live_snapshot())
    clock.advance(STATE_SNAPSHOT_MAX_AGE_S + 1.0)
    assert vs.armed is False

    # A router restart must not leave the view permanently withheld.
    vs.update_from_dict(_live_snapshot())
    assert vs.stale is False
    assert vs.armed is True


def test_update_from_dict_can_clear_a_field() -> None:
    clock = _Clock()
    vs = IpcVehicleState(clock=clock)
    vs.update_from_dict({"armed": True, "mode": "GUIDED", "fc_connected": True})
    assert vs.armed is True

    # Same tick, so the age gate plays no part here: a publication that no
    # longer carries `armed` has to take it back down.
    vs.update_from_dict({"mode": "GUIDED", "fc_connected": True})
    assert vs.armed is False
    assert vs.mode == "GUIDED"

    # And an empty publication clears the lot. This was dropped by an `if d:`
    # guard, so no publication could ever clear anything at all.
    vs.update_from_dict({})
    assert vs.snapshot == {}
    assert IpcFcConnection(vs).connected is False


def test_none_means_no_update_and_leaves_the_held_snapshot_alone() -> None:
    clock = _Clock()
    vs = IpcVehicleState(clock=clock)
    vs.update_from_dict({"armed": True})
    clock.advance(1.0)

    vs.update_from_dict(None)
    assert vs.armed is True
    # A non-update must not refresh the arrival stamp, or "nothing arrived"
    # would read as "something arrived" and defeat the age bound.
    assert vs.age_s == 1.0


def test_a_view_that_never_received_anything_is_stale_not_zeroed() -> None:
    vs = IpcVehicleState(clock=_Clock())
    assert vs.age_s is None
    assert vs.stale is True
    assert vs.snapshot == {}
    assert vs.to_dict()["battery"]["remaining"] == -1


def test_a_frozen_fix_under_a_live_heartbeat_is_not_a_current_position() -> None:
    # The case no snapshot-level age bound can catch: the router is publishing
    # at 10 Hz, so the snapshot is fresh, but POSITION stopped decoding minutes
    # ago and lat/lon have not moved since. Anything that stamps a position
    # somewhere durable has to be able to tell.
    vs = IpcVehicleState(clock=_Clock())
    vs.update_from_dict(
        {
            "position": {"lat": 12.9716, "lon": 77.5946, "alt_rel": 60.0},
            "last_update": "2026-01-01T00:00:00Z",
            "position_age_ms": POSITION_FRESH_MAX_AGE_MS + 1.0,
        }
    )

    assert vs.stale is False
    assert vs.position_age_ms == POSITION_FRESH_MAX_AGE_MS + 1.0
    assert vs.position_fresh is False


def test_a_current_fix_reads_as_fresh() -> None:
    vs = IpcVehicleState(clock=_Clock())
    vs.update_from_dict(
        {"position": {"lat": 12.9716, "lon": 77.5946}, "position_age_ms": 180}
    )
    assert vs.position_fresh is True
    assert vs.position_age_ms == 180.0


def test_an_unknown_position_age_is_never_reported_as_fresh() -> None:
    vs = IpcVehicleState(clock=_Clock())

    # No POSITION has ever arrived: the router publishes null.
    vs.update_from_dict({"position": {"lat": 0.0, "lon": 0.0}, "position_age_ms": None})
    assert vs.position_age_ms is None
    assert vs.position_fresh is False

    # A snapshot with no such key at all, and a garbage value, must land the
    # same way rather than defaulting to "current".
    vs.update_from_dict({"position": {"lat": 1.0, "lon": 2.0}})
    assert vs.position_fresh is False
    vs.update_from_dict({"position_age_ms": "soon"})
    assert vs.position_age_ms is None
    assert vs.position_fresh is False

    # A bool is the nastiest of these: `bool` subclasses `int`, so `float(True)`
    # is 1.0 and this read as a one-millisecond-old fix — "cannot tell"
    # arriving as "current", on the one accessor written to prevent that.
    for value in (True, False):
        vs.update_from_dict({"position_age_ms": value})
        assert vs.position_age_ms is None
        assert vs.position_fresh is False
