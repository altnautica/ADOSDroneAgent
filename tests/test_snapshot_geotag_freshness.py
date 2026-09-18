"""A photograph is never geotagged with a position nobody can vouch for.

A snapshot's coordinates go into the JPEG's EXIF, which makes them durable in
a way a telemetry surface is not: a stale reading on a live dial is corrected
by the next sample, but a wrong coordinate baked into a photo is wrong forever
and will be trusted later by someone with no way to know the fix was frozen.

Two failure modes existed and both are covered here:

  * A node whose GPS died while its heartbeat continued. The state snapshot is
    genuinely fresh — the router is still publishing at 10 Hz — and only the
    position's own age separates a live fix from a frozen one.
  * A node with no telemetry at all, which fell back to `0.0` and stamped
    every photo at 0°N 0°E. That is a real place in the Gulf of Guinea, not an
    obvious sentinel, so nothing downstream could tell it was fabricated.
"""

from __future__ import annotations

from dataclasses import dataclass

from ados.api.routes.video.snapshot import _fresh_position


@dataclass
class _State:
    lat: float | None = 12.9716
    lon: float | None = 77.5946
    position_fresh: bool = True


def test_a_fresh_fix_is_passed_through() -> None:
    assert _fresh_position(_State()) == (12.9716, 77.5946)


def test_a_stale_fix_is_withheld() -> None:
    # The GPS died under a live heartbeat. Withholding is honest; the last
    # known position is not, because nothing in the file would say so.
    assert _fresh_position(_State(position_fresh=False)) == (None, None)


def test_no_telemetry_at_all_is_withheld_rather_than_zeroed() -> None:
    assert _fresh_position(None) == (None, None)


def test_a_state_that_cannot_answer_freshness_is_treated_as_stale() -> None:
    # An older snapshot shape with no `position_fresh` at all. "Cannot tell"
    # must never read as "current".

    class _Legacy:
        lat = 12.9716
        lon = 77.5946

    assert _fresh_position(_Legacy()) == (None, None)


def test_a_non_numeric_coordinate_is_withheld() -> None:
    assert _fresh_position(_State(lat=None)) == (None, None)
    assert _fresh_position(_State(lon="77.5946")) == (None, None)  # type: ignore[arg-type]


def test_a_boolean_is_not_a_coordinate() -> None:
    # `bool` subclasses `int` in Python, so a bare numeric check accepts
    # `True` as latitude 1.0 — the same trap that reads a JSON `true` as a
    # one-millisecond-old fix.
    assert _fresh_position(_State(lat=True)) == (None, None)  # type: ignore[arg-type]
    assert _fresh_position(_State(lon=False)) == (None, None)  # type: ignore[arg-type]


def test_the_equator_and_prime_meridian_are_real_positions() -> None:
    # The old capture guard was `if lat != 0.0 or lon != 0.0`, which refused
    # to geotag a photo taken at exactly 0,0 while happily accepting a
    # fabricated 0,0 from a node with no fix. Zero is a coordinate; absence
    # is None.
    assert _fresh_position(_State(lat=0.0, lon=0.0)) == (0.0, 0.0)
