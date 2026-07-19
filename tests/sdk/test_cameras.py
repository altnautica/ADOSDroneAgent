"""Tests for the camera-selector resolver (the plugin camera-binding contract)."""

from __future__ import annotations

from ados.sdk import (
    CAMERA_SELECTOR_AUTO,
    primary_camera_id,
    resolve_camera_selection,
)


def _roster() -> list[dict]:
    """A representative roster: a primary EO feed, a down-facing detect cam, an
    offline belly cam, and a disabled thermal cam."""
    return [
        {
            "id": "eo",
            "role": "primary",
            "purpose": ["feed"],
            "orientation": "forward",
            "enabled": True,
            "state": "assigned",
        },
        {
            "id": "nadir",
            "role": None,
            "purpose": ["detect", "precision-landing"],
            "orientation": "down",
            "enabled": True,
            "state": "assigned",
        },
        {
            "id": "belly",
            "role": None,
            "purpose": ["detect"],
            "orientation": "down",
            "enabled": True,
            "state": "offline",
        },
        {
            "id": "thermal",
            "role": None,
            "purpose": ["thermal"],
            "orientation": "forward",
            "enabled": False,
            "state": "assigned",
        },
    ]


def test_explicit_returns_the_picked_available_camera():
    assert resolve_camera_selection("nadir", _roster()) == "nadir"


def test_explicit_miss_returns_none_when_absent():
    assert resolve_camera_selection("does-not-exist", _roster()) is None


def test_explicit_miss_returns_none_when_offline():
    # The operator pinned a camera that is now offline → None (the safety plugin
    # surfaces "no camera" rather than silently binding elsewhere).
    assert resolve_camera_selection("belly", _roster()) is None


def test_explicit_miss_returns_none_when_disabled():
    assert resolve_camera_selection("thermal", _roster()) is None


def test_by_requirement_auto_sentinel_resolves_by_purpose():
    # Auto + purpose "detect" → the first available detect cam (nadir; belly is
    # offline so it is skipped).
    assert (
        resolve_camera_selection(CAMERA_SELECTOR_AUTO, _roster(), purpose="detect")
        == "nadir"
    )


def test_by_requirement_empty_and_none_behave_like_auto():
    for value in ("", None):
        assert (
            resolve_camera_selection(value, _roster(), purpose="detect") == "nadir"
        )


def test_by_requirement_filters_by_orientation():
    # purpose detect AND orientation down → nadir (belly offline).
    assert (
        resolve_camera_selection(
            CAMERA_SELECTOR_AUTO, _roster(), purpose="detect", orientation="down"
        )
        == "nadir"
    )
    # A declared requirement nothing available satisfies (no up-facing detect cam)
    # is a HARD filter → None, never a fall-back to the primary.
    assert (
        resolve_camera_selection(
            CAMERA_SELECTOR_AUTO, _roster(), purpose="detect", orientation="up"
        )
        is None
    )


def test_by_requirement_hard_filter_returns_none_when_no_match():
    # No thermal-down cam available → the declared requirement is a hard filter,
    # so the resolution returns None (the safety plugin stops) rather than binding
    # to the primary EO leg, which does not meet the requirement.
    assert (
        resolve_camera_selection(
            CAMERA_SELECTOR_AUTO, _roster(), purpose="thermal", orientation="down"
        )
        is None
    )
    # A declared purpose with no available match → None even though a primary EO
    # leg is available.
    assert (
        resolve_camera_selection(CAMERA_SELECTOR_AUTO, _roster(), purpose="mapping")
        is None
    )


def test_by_requirement_with_no_purpose_returns_the_primary():
    # With NO requirement declared, auto falls back to the primary available leg.
    assert resolve_camera_selection(CAMERA_SELECTOR_AUTO, _roster()) == "eo"


def test_empty_roster_resolves_to_none():
    assert resolve_camera_selection(CAMERA_SELECTOR_AUTO, [], purpose="detect") is None
    assert resolve_camera_selection("eo", []) is None


def test_primary_camera_id_prefers_the_primary_role():
    assert primary_camera_id(_roster()) == "eo"


def test_primary_camera_id_returns_first_available_or_none():
    # No primary role → the first available leg.
    cams = [
        {"id": "a", "role": None, "enabled": False, "state": "assigned"},
        {"id": "b", "role": None, "enabled": True, "state": "assigned"},
    ]
    assert primary_camera_id(cams) == "b"
    # None available → None (never a dead camera as a last resort).
    cams = [
        {"id": "a", "role": None, "enabled": False, "state": "offline"},
        {"id": "b", "role": None, "enabled": False, "state": "offline"},
    ]
    assert primary_camera_id(cams) is None


def test_all_offline_roster_resolves_to_none():
    # Every leg offline/disabled → auto (no requirement) resolves to None, and
    # primary_camera_id resolves to None: a safety plugin stops rather than
    # binding to a dead camera.
    cams = [
        {"id": "a", "role": "primary", "enabled": True, "state": "offline"},
        {"id": "b", "role": None, "enabled": False, "state": "assigned"},
    ]
    assert resolve_camera_selection(CAMERA_SELECTOR_AUTO, cams) is None
    assert primary_camera_id(cams) is None


def test_lenient_defaults_treat_missing_fields_as_available():
    # A hand-built list with only ids (no enabled/state) is treated as available.
    cams = [{"id": "cam0", "purpose": ["detect"]}]
    assert resolve_camera_selection("cam0", cams) == "cam0"
    assert resolve_camera_selection(CAMERA_SELECTOR_AUTO, cams, purpose="detect") == "cam0"
