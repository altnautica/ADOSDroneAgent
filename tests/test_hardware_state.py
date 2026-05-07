"""Tests for the persistent hardware-check snapshot.

Covers the read/write/is_fresh helpers in
``ados.setup.hardware_state`` plus the cache hit/miss/profile-swap
behavior of ``run_hardware_check_cached``.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import pytest

from ados.setup import hardware_check, hardware_state
from ados.setup.models import HardwareCheckItem, HardwareCheckStatus


def _make_status(
    profile: str = "drone",
    ground_role: str = "",
    last_run: str | None = None,
    item_state: str = "ok",
) -> HardwareCheckStatus:
    if last_run is None:
        last_run = datetime.now(timezone.utc).isoformat()
    return HardwareCheckStatus(
        profile=profile,
        ground_role=ground_role,
        items=[
            HardwareCheckItem(
                id="board",
                label="Companion compute",
                required=True,
                state=item_state,
                detail="Test board",
            )
        ],
        last_run=last_run,
    )


def _redirect_state_path(monkeypatch, tmp_path) -> None:
    target = tmp_path / "hardware-state.json"
    monkeypatch.setattr(hardware_state, "HARDWARE_STATE_PATH", target)
    monkeypatch.setattr(hardware_state, "SETUP_STATE_DIR", tmp_path)


def test_read_returns_none_when_no_file(monkeypatch, tmp_path) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    assert hardware_state.read() is None


def test_write_then_read_round_trips(monkeypatch, tmp_path) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    status = _make_status()
    hardware_state.write(status)
    loaded = hardware_state.read()
    assert loaded is not None
    assert loaded.profile == "drone"
    assert len(loaded.items) == 1
    assert loaded.items[0].id == "board"


def test_read_returns_none_on_corrupt_json(monkeypatch, tmp_path) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    hardware_state.HARDWARE_STATE_PATH.write_text("{not json")
    assert hardware_state.read() is None


def test_is_fresh_true_for_recent(monkeypatch, tmp_path) -> None:
    status = _make_status(last_run=datetime.now(timezone.utc).isoformat())
    assert hardware_state.is_fresh(status, ttl_seconds=60)


def test_is_fresh_false_for_stale() -> None:
    old = (datetime.now(timezone.utc) - timedelta(minutes=5)).isoformat()
    status = _make_status(last_run=old)
    assert not hardware_state.is_fresh(status, ttl_seconds=30)


def test_is_fresh_false_for_empty_timestamp() -> None:
    status = _make_status(last_run="")
    assert not hardware_state.is_fresh(status, ttl_seconds=30)


def test_is_fresh_false_for_unparseable_timestamp() -> None:
    status = _make_status(last_run="not-a-timestamp")
    assert not hardware_state.is_fresh(status, ttl_seconds=30)


def test_matches_profile_and_role() -> None:
    status = _make_status(profile="drone", ground_role="")
    assert hardware_state.matches(status, profile="drone", ground_role="")
    assert not hardware_state.matches(
        status, profile="ground_station", ground_role="direct"
    )


def test_clear_removes_file(monkeypatch, tmp_path) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    hardware_state.write(_make_status())
    assert hardware_state.HARDWARE_STATE_PATH.is_file()
    hardware_state.clear()
    assert not hardware_state.HARDWARE_STATE_PATH.is_file()


def test_cached_runner_hits_cache_on_repeat_call(
    monkeypatch, tmp_path
) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    probe_count = {"n": 0}
    sentinel = _make_status()

    def _fake_fresh(runtime, *, profile, ground_role=None):
        probe_count["n"] += 1
        return sentinel

    monkeypatch.setattr(
        hardware_check, "run_hardware_check_fresh", _fake_fresh
    )

    a = hardware_check.run_hardware_check_cached(None, profile="drone")
    b = hardware_check.run_hardware_check_cached(None, profile="drone")
    assert a is sentinel
    assert b.profile == "drone"
    assert probe_count["n"] == 1, "second call should read from cache"


def test_cached_runner_reprobes_after_ttl_expires(
    monkeypatch, tmp_path
) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    probe_count = {"n": 0}

    def _fake_fresh(runtime, *, profile, ground_role=None):
        probe_count["n"] += 1
        return _make_status()

    monkeypatch.setattr(
        hardware_check, "run_hardware_check_fresh", _fake_fresh
    )

    hardware_check.run_hardware_check_cached(None, profile="drone")
    # Force-stale the persisted snapshot so the next call must reprobe.
    stale = _make_status(
        last_run=(
            datetime.now(timezone.utc) - timedelta(minutes=10)
        ).isoformat()
    )
    hardware_state.write(stale)
    hardware_check.run_hardware_check_cached(None, profile="drone")
    assert probe_count["n"] == 2


def test_cached_runner_invalidates_on_profile_swap(
    monkeypatch, tmp_path
) -> None:
    _redirect_state_path(monkeypatch, tmp_path)
    probe_count = {"n": 0}

    def _fake_fresh(runtime, *, profile, ground_role=None):
        probe_count["n"] += 1
        return _make_status(
            profile=profile, ground_role=ground_role or ""
        )

    monkeypatch.setattr(
        hardware_check, "run_hardware_check_fresh", _fake_fresh
    )

    hardware_check.run_hardware_check_cached(None, profile="drone")
    # Same profile, second call: cache hit.
    hardware_check.run_hardware_check_cached(None, profile="drone")
    assert probe_count["n"] == 1
    # Profile swap: must reprobe.
    hardware_check.run_hardware_check_cached(
        None, profile="ground_station", ground_role="direct"
    )
    assert probe_count["n"] == 2
