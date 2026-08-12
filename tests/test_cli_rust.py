"""Tests for the ``ados rust`` cutover-toggle subcommand."""

from __future__ import annotations

import pytest
from click.testing import CliRunner

from ados.cli import rust as rust_mod
from ados.cli.rust import _SERVICES, _SVC_NAMES, rust_group


@pytest.fixture(autouse=True)
def _force_linux(monkeypatch):
    """The native-vs-packaged cutover only applies to a systemd-managed agent,
    so pin the platform to Linux to exercise that path regardless of the host the
    tests run on (on macOS the commands short-circuit with a not-applicable note)."""
    monkeypatch.setattr(rust_mod.platform, "system", lambda: "Linux")


def test_service_map_is_well_formed():
    """Every service has a flag, at least one binary, and is reachable by
    name. The flag names must match the sentinel files the units check:
    opt-in services use a ``*-rust-enabled`` marker, an opt-out (already
    cut over) service uses a ``*-fallback`` marker."""
    assert set(_SVC_NAMES) == set(_SERVICES)
    for name, svc in _SERVICES.items():
        if svc.opt_out:
            assert svc.flag.endswith("-fallback"), name
        else:
            assert svc.flag.endswith("-rust-enabled"), name
        assert svc.binaries, name
        # A service is either a swap (both impls in one unit) or carries
        # native-only extra units — never neither.
        assert svc.swap_units or svc.extra_units, name


def test_native_only_services_are_absent_from_the_toggle_registry():
    """A service with no packaged implementation left must not be listed here.

    The registry drives `ados rust enable/disable`, so a name in it advertises a
    fallback an operator can switch to. Once the packaged side is deleted there
    is nothing to switch to, and offering the toggle would write a marker that
    selects nothing -- on the plugin host that previously meant an ExecStart of
    /bin/true, i.e. no host running at all. On display it meant something quieter
    and longer-lived: the marker changed nothing about what ran, but flipped the
    whole node's runtime badge to "hybrid" for as long as it sat there.
    """
    for name in ("net", "hid", "plugin-host", "display"):
        assert name not in _SERVICES, (
            f"{name} has no packaged fallback left; listing it offers a toggle "
            "that would pin a box to an implementation that no longer exists"
        )


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


def test_enable_requires_root(tmp_path, monkeypatch):
    """enable touches /etc/ados and drives systemctl, so a non-root caller
    is refused before anything is written."""
    monkeypatch.setattr(rust_mod, "ADOS_ETC_DIR", tmp_path)
    monkeypatch.setattr(rust_mod.os, "geteuid", lambda: 1000)
    monkeypatch.setattr(rust_mod, "_binaries_present", lambda svc: True)
    result = CliRunner().invoke(rust_group, ["enable", "control"])
    assert result.exit_code != 0
    assert "sudo" in result.output.lower()
    assert not (tmp_path / _SERVICES["control"].flag).exists()


