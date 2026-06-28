"""Tests for the plugin-side perception-offload safety primitives.

Mirrors the agent's ados-offload gate: freshness (age + link) and the
lock-state machine, with the never-auto-re-acquire invariant.
"""

from __future__ import annotations

from ados.sdk.offload import FreshnessGate, GateState, LockGate, LockState


def test_freshness_is_lost_until_a_result_then_fresh_then_stale():
    g = FreshnessGate(budget_ms=500)
    assert g.state(1000) is GateState.LOST  # no result yet
    g.record(1000)
    assert g.state(1100) is GateState.FRESH  # 100 <= 500
    assert g.state(1501) is GateState.STALE  # 501 > 500
    assert not g.is_usable(1501)


def test_a_link_drop_is_lost_even_with_a_fresh_result():
    g = FreshnessGate(budget_ms=500)
    g.record(1000)
    g.set_link(False)
    assert g.state(1050) is GateState.LOST
    g.set_link(True)
    assert g.state(1050) is GateState.FRESH


def test_a_frozen_stream_goes_stale_on_local_elapsed():
    # The safety regression mirror: anchored on local arrival, a stream that
    # stops arriving goes stale once the local clock passes the budget.
    g = FreshnessGate(budget_ms=500)
    g.record(1000)
    assert g.state(1400) is GateState.FRESH
    assert g.state(1501) is GateState.STALE
    assert g.state(100_000) is GateState.STALE


def test_the_budget_boundary_is_inclusive():
    g = FreshnessGate(budget_ms=500)
    g.record(1000)
    assert g.state(1500) is GateState.FRESH  # age == budget
    assert g.state(1501) is GateState.STALE  # age == budget + 1


def test_a_negative_budget_is_clamped():
    g = FreshnessGate(budget_ms=-100)
    g.record(1000)
    assert g.state(1000) is GateState.FRESH
    assert g.state(1001) is GateState.STALE


def test_lock_gate_acquiring_stays_locked_not_lost():
    g = LockGate()
    g.lock()
    g.update(False)  # no usable reading yet -> acquiring, not lost
    g.update(False)
    assert g.state is LockState.LOCKED


def test_lock_gate_drops_to_lost_after_tracking_then_stale():
    g = LockGate()
    g.lock()
    g.update(True)  # acquired
    g.update(False)  # then stale -> lost
    assert g.state is LockState.LOST
    assert not g.is_locked


def test_lock_gate_never_auto_re_acquires():
    g = LockGate()
    g.lock()
    g.update(True)
    g.update(False)  # -> lost
    assert g.state is LockState.LOST
    # Fresh again, but it must NOT re-lock by itself.
    g.update(True)
    g.update(True)
    assert g.state is LockState.LOST
    # Only an explicit re-designate re-acquires.
    g.lock()
    assert g.state is LockState.LOCKED


def test_an_idle_gate_does_not_lock_itself():
    g = LockGate()
    g.update(True)
    assert g.state is LockState.UNLOCKED
