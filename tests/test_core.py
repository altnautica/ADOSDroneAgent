"""Tests for service state machine and ServiceTracker."""

from __future__ import annotations

import time

from ados.core.main import ServiceState, ServiceTracker


def test_service_state_enum_values():
    """All expected states exist with correct string values."""
    assert ServiceState.STOPPED == "stopped"
    assert ServiceState.STARTING == "starting"
    assert ServiceState.RUNNING == "running"
    assert ServiceState.DEGRADED == "degraded"
    assert ServiceState.FAILED == "failed"


def test_tracker_initial_state():
    """Unknown services default to STOPPED."""
    tracker = ServiceTracker()
    assert tracker.get_state("nonexistent") == ServiceState.STOPPED


def test_tracker_set_and_get():
    """Setting state should be retrievable."""
    tracker = ServiceTracker()
    tracker.set_state("fc-connection", ServiceState.STARTING)
    assert tracker.get_state("fc-connection") == ServiceState.STARTING


def test_tracker_transitions_recorded():
    """Each set_state call should record a transition."""
    tracker = ServiceTracker()
    tracker.set_state("api", ServiceState.STARTING)
    tracker.set_state("api", ServiceState.RUNNING)
    tracker.set_state("api", ServiceState.DEGRADED)

    transitions = tracker.get_transitions("api")
    assert len(transitions) == 3
    assert transitions[0][1] == ServiceState.STARTING
    assert transitions[1][1] == ServiceState.RUNNING
    assert transitions[2][1] == ServiceState.DEGRADED


def test_tracker_transition_timestamps_increase():
    """Transition timestamps should be monotonically increasing."""
    tracker = ServiceTracker()
    tracker.set_state("svc", ServiceState.STARTING)
    tracker.set_state("svc", ServiceState.RUNNING)

    transitions = tracker.get_transitions("svc")
    assert transitions[1][0] >= transitions[0][0]


def test_tracker_get_all():
    """get_all should return all tracked services."""
    tracker = ServiceTracker()
    tracker.set_state("a", ServiceState.RUNNING)
    tracker.set_state("b", ServiceState.FAILED)

    all_states = tracker.get_all()
    assert all_states["a"] == ServiceState.RUNNING
    assert all_states["b"] == ServiceState.FAILED


def test_tracker_to_dict():
    """to_dict should serialize for REST API."""
    tracker = ServiceTracker()
    tracker.set_state("svc", ServiceState.STARTING)
    tracker.set_state("svc", ServiceState.RUNNING)

    d = tracker.to_dict()
    assert "svc" in d
    assert d["svc"]["state"] == "running"
    assert d["svc"]["transition_count"] == 2
    assert d["svc"]["last_transition"] > 0


def test_tracker_multiple_services():
    """Multiple services tracked independently."""
    tracker = ServiceTracker()
    tracker.set_state("fc", ServiceState.RUNNING)
    tracker.set_state("api", ServiceState.STARTING)
    tracker.set_state("mqtt", ServiceState.FAILED)

    assert tracker.get_state("fc") == ServiceState.RUNNING
    assert tracker.get_state("api") == ServiceState.STARTING
    assert tracker.get_state("mqtt") == ServiceState.FAILED


def test_tracker_empty_transitions():
    """get_transitions for untracked service returns empty list."""
    tracker = ServiceTracker()
    assert tracker.get_transitions("unknown") == []


def test_tracker_overwrite_state():
    """Setting state multiple times updates the current state."""
    tracker = ServiceTracker()
    tracker.set_state("svc", ServiceState.RUNNING)
    tracker.set_state("svc", ServiceState.FAILED)
    tracker.set_state("svc", ServiceState.STOPPED)
    assert tracker.get_state("svc") == ServiceState.STOPPED
