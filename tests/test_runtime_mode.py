"""Tests for the native-vs-packaged runtime-mode aggregate.

Exercises :func:`ados.core.runtime_mode.compute_runtime_mode` against a
temporary ``/opt/ados/bin`` + ``/etc/ados`` layout so the per-service
verdicts and the profile scoping are covered without touching the real
install. Also asserts the heartbeat + full-status payloads carry the
``runtimeMode`` field once the helper is wired in.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from ados.core.runtime_mode import compute_runtime_mode

# Core binaries that run native whenever present (no flag gate).
_CORE = ("ados-supervisor", "ados-video", "ados-cloud", "ados-mavlink-router")


def _make_bin(bin_dir: Path, name: str) -> None:
    """Create an executable stub binary under bin_dir."""
    bin_dir.mkdir(parents=True, exist_ok=True)
    p = bin_dir / name
    p.write_text("#!/bin/sh\n")
    p.chmod(0o755)


def _touch_flag(etc_dir: Path, name: str) -> None:
    etc_dir.mkdir(parents=True, exist_ok=True)
    (etc_dir / name).touch()


@pytest.fixture()
def roots(tmp_path: Path) -> tuple[Path, Path]:
    """A clean bin_dir + etc_dir pair under a tmp path."""
    return tmp_path / "opt-ados-bin", tmp_path / "etc-ados"


def test_packaged_when_no_binaries(roots: tuple[Path, Path]) -> None:
    """No native binaries anywhere → a pre-cutover Python-only agent."""
    bin_dir, etc_dir = roots
    bin_dir.mkdir(parents=True, exist_ok=True)
    etc_dir.mkdir(parents=True, exist_ok=True)
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "packaged"
    assert (
        compute_runtime_mode("ground-station", bin_dir=bin_dir, etc_dir=etc_dir)
        == "packaged"
    )


def test_missing_dirs_are_total(roots: tuple[Path, Path]) -> None:
    """Absent bin/etc dirs resolve to packaged, never raise."""
    bin_dir, etc_dir = roots  # never created
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "packaged"


def test_hybrid_core_only(roots: tuple[Path, Path]) -> None:
    """Core binaries present, no flag-gated services enabled → hybrid."""
    bin_dir, etc_dir = roots
    for b in _CORE:
        _make_bin(bin_dir, b)
    # Drone: radio + plugin-host + display + net flag-gated, none enabled.
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "hybrid"


def test_hybrid_when_flag_set_but_binary_missing(roots: tuple[Path, Path]) -> None:
    """A flag with no binary stays packaged for that service → hybrid."""
    bin_dir, etc_dir = roots
    for b in _CORE:
        _make_bin(bin_dir, b)
    # All drone flag-gated flags set, but their binaries are absent.
    for flag in ("wfb-rust-enabled", "plugin-host-rust-enabled",
                 "display-rust-enabled", "net-rust-enabled"):
        _touch_flag(etc_dir, flag)
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "hybrid"


def test_native_drone_all_applicable(roots: tuple[Path, Path]) -> None:
    """Drone: core + radio + net + plugin-host + display native → native.

    The drone profile excludes the GS-only services (groundlink, hid), so
    they must not hold the drone back.
    """
    bin_dir, etc_dir = roots
    for b in _CORE:
        _make_bin(bin_dir, b)
    for b in ("ados-radio", "ados-net", "ados-plugin-host",
              "ados-display", "ados-display-probe"):
        _make_bin(bin_dir, b)
    for flag in ("wfb-rust-enabled", "net-rust-enabled",
                 "plugin-host-rust-enabled", "display-rust-enabled"):
        _touch_flag(etc_dir, flag)
    # groundlink + hid binaries deliberately absent — they are not in the
    # drone applicable set, so the drone is still native.
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "native"


def test_gs_needs_groundlink_and_hid(roots: tuple[Path, Path]) -> None:
    """Ground station: the drone-native layout above is still hybrid for a
    GS because groundlink + hid are applicable there and not yet native."""
    bin_dir, etc_dir = roots
    for b in _CORE:
        _make_bin(bin_dir, b)
    for b in ("ados-net", "ados-plugin-host", "ados-display", "ados-display-probe"):
        _make_bin(bin_dir, b)
    for flag in ("net-rust-enabled", "plugin-host-rust-enabled",
                 "display-rust-enabled"):
        _touch_flag(etc_dir, flag)
    # groundlink + hid not present → GS is hybrid.
    assert (
        compute_runtime_mode("ground-station", bin_dir=bin_dir, etc_dir=etc_dir)
        == "hybrid"
    )

    # Now make groundlink + hid native too → GS reaches native.
    for b in ("ados-groundlink", "ados-pic", "ados-input"):
        _make_bin(bin_dir, b)
    for flag in ("groundlink-rust-enabled", "hid-rust-enabled"):
        _touch_flag(etc_dir, flag)
    assert (
        compute_runtime_mode("ground-station", bin_dir=bin_dir, etc_dir=etc_dir)
        == "native"
    )


def test_radio_only_present_is_not_packaged(roots: tuple[Path, Path]) -> None:
    """A single flag-gated binary present (no core) still counts as
    'some native binary present' → not packaged."""
    bin_dir, etc_dir = roots
    _make_bin(bin_dir, "ados-radio")
    # No core binaries, radio flag not set → hybrid (radio not native, core
    # not native), but crucially not 'packaged' since a binary exists.
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "hybrid"


@pytest.mark.parametrize("value", ["native", "hybrid", "packaged"])
def test_returns_only_valid_values(roots: tuple[Path, Path], value: str) -> None:
    """Sanity: the return is always one of the three known labels."""
    bin_dir, etc_dir = roots
    result = compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir)
    assert result in ("native", "hybrid", "packaged")


def test_default_roots_do_not_raise() -> None:
    """Calling with the real default paths is total (the CI box has no
    /opt/ados/bin) and returns a valid label."""
    assert compute_runtime_mode("drone") in ("native", "hybrid", "packaged")


def test_runtime_mode_enrichment_shape() -> None:
    """The cloud-heartbeat sibling enricher returns the runtimeMode key."""
    from ados.services.cloud.heartbeat import build_runtime_mode_enrichment

    out = build_runtime_mode_enrichment(object())
    assert set(out) == {"runtimeMode"}
    assert out["runtimeMode"] in ("native", "hybrid", "packaged")
