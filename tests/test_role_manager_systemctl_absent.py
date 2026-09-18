"""A role transition must not report success when `systemctl` is not there.

`_run_systemctl` swallowed `FileNotFoundError` and returned success "so the
function stays callable under pytest without mocking". The cost: on any box
without systemd, `apply_role` masked nothing, unmasked nothing, started nothing,
and still wrote the sentinel and reported the units it claimed to have started —
a node wired for `direct` while every surface said `relay`.
"""

from __future__ import annotations

import subprocess

import pytest

from ados.services.ground_station import role_manager as rm


@pytest.fixture
def no_systemctl(tmp_path, monkeypatch):
    """A box with no systemctl, and a sentinel + state files under tmp."""

    def _raise(cmd, **kwargs):
        raise FileNotFoundError(2, "No such file or directory", "systemctl")

    monkeypatch.setattr(rm.subprocess, "run", _raise)
    monkeypatch.setattr(rm, "ROLE_FILE", tmp_path / "role")
    monkeypatch.setattr(rm, "_MESH_STATE_FILES", ())
    return tmp_path


def test_a_missing_systemctl_is_reported_as_a_failure(no_systemctl) -> None:
    ok, detail = rm._run_systemctl(["start", "ados-batman.service"])
    assert ok is False
    assert detail


async def test_a_transition_that_started_nothing_claims_nothing(no_systemctl) -> None:
    result = await rm.apply_role("relay", previous="direct")

    assert result["role"] == "relay"
    # The units for the relay role exist in the table; none of them started, so
    # none may be reported as started.
    assert rm.role_units("relay")
    assert result["units_started"] == []


def test_a_timeout_is_still_reported_as_a_failure(tmp_path, monkeypatch) -> None:
    def _timeout(cmd, **kwargs):
        raise subprocess.TimeoutExpired(cmd, 15.0)

    monkeypatch.setattr(rm.subprocess, "run", _timeout)
    ok, detail = rm._run_systemctl(["start", "ados-batman.service"])
    assert ok is False
    assert detail == "timeout"
