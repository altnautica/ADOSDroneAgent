"""Tests for the ``ados rust`` cutover-toggle subcommand."""

from __future__ import annotations

from click.testing import CliRunner

from ados.cli import rust as rust_mod
from ados.cli.rust import _SERVICES, _SVC_NAMES, rust_group


def test_service_map_is_well_formed():
    """Every service has a flag, at least one binary, and is reachable by
    name. The flag names must match the sentinel files the units check."""
    assert set(_SVC_NAMES) == set(_SERVICES)
    for name, svc in _SERVICES.items():
        assert svc.flag.endswith("-rust-enabled"), name
        assert svc.binaries, name
        # A service is either a swap (both impls in one unit) or carries
        # native-only extra units — never neither.
        assert svc.swap_units or svc.extra_units, name


def test_status_reports_python_when_no_flags(tmp_path, monkeypatch):
    """With no flag files and no installed binaries, every service reports
    the packaged (python) branch and the command exits clean for any user."""
    monkeypatch.setattr(rust_mod, "ADOS_ETC_DIR", tmp_path)
    monkeypatch.setattr(rust_mod, "_binaries_present", lambda svc: False)
    monkeypatch.setattr(rust_mod, "_unit_active", lambda unit: False)
    result = CliRunner().invoke(rust_group, ["status"])
    assert result.exit_code == 0, result.output
    assert "python" in result.output
    for name in _SVC_NAMES:
        assert name in result.output


def test_status_reports_rust_when_flag_and_binary_present(tmp_path, monkeypatch):
    """A set flag plus an installed binary makes the unit take the native
    branch, and status must reflect that as ``rust``."""
    monkeypatch.setattr(rust_mod, "ADOS_ETC_DIR", tmp_path)
    (tmp_path / _SERVICES["net"].flag).touch()
    monkeypatch.setattr(rust_mod, "_binaries_present", lambda svc: svc is _SERVICES["net"])
    monkeypatch.setattr(rust_mod, "_unit_active", lambda unit: True)
    result = CliRunner().invoke(rust_group, ["status"])
    assert result.exit_code == 0, result.output
    assert "rust" in result.output


def test_enable_requires_root(tmp_path, monkeypatch):
    """enable touches /etc/ados and drives systemctl, so a non-root caller
    is refused before anything is written."""
    monkeypatch.setattr(rust_mod, "ADOS_ETC_DIR", tmp_path)
    monkeypatch.setattr(rust_mod.os, "geteuid", lambda: 1000)
    monkeypatch.setattr(rust_mod, "_binaries_present", lambda svc: True)
    result = CliRunner().invoke(rust_group, ["enable", "net"])
    assert result.exit_code != 0
    assert "sudo" in result.output.lower()
    assert not (tmp_path / _SERVICES["net"].flag).exists()


def test_enable_writes_flag_and_reconciles_subsumed(tmp_path, monkeypatch):
    """enabling net writes the flag, restarts the swap unit, and masks the
    three packaged units the native uplink daemon absorbs."""
    monkeypatch.setattr(rust_mod, "ADOS_ETC_DIR", tmp_path)
    monkeypatch.setattr(rust_mod.os, "geteuid", lambda: 0)
    monkeypatch.setattr(rust_mod, "_binaries_present", lambda svc: True)
    monkeypatch.setattr(rust_mod, "_unit_active", lambda unit: True)
    calls: list[tuple[str, ...]] = []
    monkeypatch.setattr(rust_mod, "_systemctl", lambda *a, **k: calls.append(a) or 0)
    result = CliRunner().invoke(rust_group, ["enable", "net"])
    assert result.exit_code == 0, result.output
    assert (tmp_path / _SERVICES["net"].flag).exists()
    assert ("restart", "ados-uplink-router") in calls
    for unit in _SERVICES["net"].subsumes:
        assert ("disable", unit) in calls


def test_disable_removes_flag_and_restores_subsumed(tmp_path, monkeypatch):
    """disabling net removes the flag and re-enables the packaged units."""
    monkeypatch.setattr(rust_mod, "ADOS_ETC_DIR", tmp_path)
    monkeypatch.setattr(rust_mod.os, "geteuid", lambda: 0)
    monkeypatch.setattr(rust_mod, "_binaries_present", lambda svc: True)
    monkeypatch.setattr(rust_mod, "_unit_active", lambda unit: False)
    (tmp_path / _SERVICES["net"].flag).touch()
    calls: list[tuple[str, ...]] = []
    monkeypatch.setattr(rust_mod, "_systemctl", lambda *a, **k: calls.append(a) or 0)
    result = CliRunner().invoke(rust_group, ["disable", "net"])
    assert result.exit_code == 0, result.output
    assert not (tmp_path / _SERVICES["net"].flag).exists()
    for unit in _SERVICES["net"].subsumes:
        assert ("enable", unit) in calls


def test_enable_kills_subsumed_unit_that_will_not_stop(tmp_path, monkeypatch):
    """A subsumed unit slow to honor SIGTERM is SIGKILLed and reset-failed so
    it ends cleanly inactive instead of lingering as failed."""
    monkeypatch.setattr(rust_mod, "ADOS_ETC_DIR", tmp_path)
    monkeypatch.setattr(rust_mod.os, "geteuid", lambda: 0)
    monkeypatch.setattr(rust_mod, "_binaries_present", lambda svc: True)
    monkeypatch.setattr(rust_mod, "_unit_active", lambda unit: True)
    calls: list[tuple[str, ...]] = []

    def fake(*a, **k):
        calls.append(a)
        # Every stop "times out" so the kill fallback must engage.
        return 124 if a and a[0] == "stop" else 0

    monkeypatch.setattr(rust_mod, "_systemctl", fake)
    result = CliRunner().invoke(rust_group, ["enable", "net"])
    assert result.exit_code == 0, result.output
    for unit in _SERVICES["net"].subsumes:
        assert ("kill", "-s", "SIGKILL", unit) in calls
        assert ("reset-failed", unit) in calls
