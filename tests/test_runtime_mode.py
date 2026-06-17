"""Tests for the native-vs-packaged runtime-mode aggregate.

Exercises :func:`ados.core.runtime_mode.compute_runtime_mode` against a
temporary ``/opt/ados/bin`` + ``/etc/ados`` layout so the per-service
verdicts and the profile scoping are covered without touching the real
install. Also asserts the heartbeat + full-status payloads carry the
``runtimeMode`` field once the helper is wired in.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from ados.core.runtime_mode import compute_runtime_mode, is_service_native

# Core binaries that run native whenever present (no flag gate).
_CORE = ("ados-supervisor", "ados-video", "ados-cloud", "ados-mavlink-router")
# The core binaries the installer fetches for a ground station — ados-video is
# drone-only (a GS receives video through ados-mediamtx-gs), so a GS that never
# installs it must still be able to reach "native".
_CORE_GS = ("ados-supervisor", "ados-cloud", "ados-mavlink-router")


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
    """Core binaries present, no flag-gated binaries present → hybrid."""
    bin_dir, etc_dir = roots
    for b in _CORE:
        _make_bin(bin_dir, b)
    # Drone: radio + net are native-only and display + plugin-host are opt-out
    # (all native once their binaries are present). With no flag-gated binaries
    # on disk none of them are native, so the aggregate stays hybrid.
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "hybrid"


def test_hybrid_when_flag_set_but_binary_missing(roots: tuple[Path, Path]) -> None:
    """An opt-in flag with no binary stays packaged for that service → hybrid.

    The remaining opt-in service is hid (ground-station only): with its enable
    marker set but its binaries absent it is still not native, so a GS otherwise
    holding only core binaries stays hybrid.
    """
    bin_dir, etc_dir = roots
    for b in _CORE:
        _make_bin(bin_dir, b)
    # The opt-in enable flag is set but its binaries are absent, so hid stays
    # packaged and the aggregate is hybrid.
    _touch_flag(etc_dir, "hid-rust-enabled")
    assert (
        compute_runtime_mode("ground-station", bin_dir=bin_dir, etc_dir=etc_dir)
        == "hybrid"
    )


def test_native_drone_all_applicable(roots: tuple[Path, Path]) -> None:
    """Drone: core + radio + plugin-host + display native → native.

    The drone profile excludes the GS-only services (net, groundlink, hid), so
    they must not hold the drone back — and crucially ados-net is NOT present
    here (the installer fetches it for the ground profile only), which must not
    pin the drone at hybrid.
    """
    bin_dir, etc_dir = roots
    for b in _CORE:
        _make_bin(bin_dir, b)
    for b in ("ados-radio", "ados-plugin-host",
              "ados-display", "ados-display-probe"):
        _make_bin(bin_dir, b)
    # radio is native-only and display + plugin-host are opt-out: all native once
    # their binaries are present, so none of them needs a marker. net/groundlink/
    # hid binaries deliberately absent — they are not in the drone applicable set,
    # so the drone is still native.
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "native"


def test_gs_needs_groundlink_and_hid(roots: tuple[Path, Path]) -> None:
    """Ground station: the drone-native layout above is still hybrid for a
    GS because groundlink + hid are applicable there and not yet native."""
    bin_dir, etc_dir = roots
    # GS core does NOT include ados-video (drone-only) — a GS that never installs
    # it must still be able to reach native.
    for b in _CORE_GS:
        _make_bin(bin_dir, b)
    for b in ("ados-net", "ados-plugin-host", "ados-display", "ados-display-probe"):
        _make_bin(bin_dir, b)
    # net is native-only and display + plugin-host are opt-out: all native once
    # their binaries are present, none needs a marker.
    # groundlink + hid not present → GS is hybrid.
    assert (
        compute_runtime_mode("ground-station", bin_dir=bin_dir, etc_dir=etc_dir)
        == "hybrid"
    )

    # Now make groundlink + hid native too → GS reaches native. groundlink
    # is native-only (native once its binary is present, no marker); hid is
    # still opt-in and needs its marker.
    for b in ("ados-groundlink", "ados-pic", "ados-input"):
        _make_bin(bin_dir, b)
    _touch_flag(etc_dir, "hid-rust-enabled")
    assert (
        compute_runtime_mode("ground-station", bin_dir=bin_dir, etc_dir=etc_dir)
        == "native"
    )

    # The mesh relay/receiver fallback marker is ignored by the direct-role
    # groundlink verdict — touching it does not drop the GS off native.
    _touch_flag(etc_dir, "groundlink-python-fallback")
    assert (
        compute_runtime_mode("ground-station", bin_dir=bin_dir, etc_dir=etc_dir)
        == "native"
    )


def test_display_opt_out_native_by_default_fallback_marker_forces_hybrid(
    roots: tuple[Path, Path],
) -> None:
    """Display is cut over: native once its binaries are present, with no
    marker. The ``display-python-fallback`` marker pins the packaged path,
    which drops the aggregate to hybrid."""
    bin_dir, etc_dir = roots
    for b in _CORE:
        _make_bin(bin_dir, b)
    for b in ("ados-radio", "ados-net", "ados-plugin-host",
              "ados-display", "ados-display-probe"):
        _make_bin(bin_dir, b)

    # Default: no fallback marker → display native → drone native.
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "native"

    # Pin the packaged fallback → display non-native → hybrid.
    _touch_flag(etc_dir, "display-python-fallback")
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "hybrid"


def test_plugin_host_opt_out_native_by_default_fallback_marker_forces_hybrid(
    roots: tuple[Path, Path],
) -> None:
    """The plugin host is cut over: native once its binary is present, with no
    marker. The ``plugin-host-python-fallback`` marker pins the packaged host
    server, which drops the aggregate to hybrid."""
    bin_dir, etc_dir = roots
    for b in _CORE:
        _make_bin(bin_dir, b)
    for b in ("ados-radio", "ados-net", "ados-plugin-host",
              "ados-display", "ados-display-probe"):
        _make_bin(bin_dir, b)
    # radio + net are native-only; display + plugin-host are opt-out (native by
    # default). None needs a marker.

    # Default: no fallback marker → plugin host native → drone native.
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "native"

    # Pin the packaged fallback → plugin host non-native → hybrid.
    _touch_flag(etc_dir, "plugin-host-python-fallback")
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "hybrid"


def test_plugin_host_opt_out_ignores_legacy_opt_in_marker(
    roots: tuple[Path, Path],
) -> None:
    """The retired opt-in marker no longer selects the plugin host. With the
    binary present and no fallback marker the host is native; a stray legacy
    ``plugin-host-rust-enabled`` marker is ignored and does not change that."""
    bin_dir, etc_dir = roots
    bin_dir.mkdir(parents=True, exist_ok=True)
    etc_dir.mkdir(parents=True, exist_ok=True)
    _make_bin(bin_dir, "ados-plugin-host")
    assert is_service_native("plugin-host", bin_dir=bin_dir, etc_dir=etc_dir)
    # A stray legacy opt-in marker is ignored — the host stays native.
    _touch_flag(etc_dir, "plugin-host-rust-enabled")
    assert is_service_native("plugin-host", bin_dir=bin_dir, etc_dir=etc_dir)
    # The fallback marker pins the packaged path → not native.
    _touch_flag(etc_dir, "plugin-host-python-fallback")
    assert not is_service_native("plugin-host", bin_dir=bin_dir, etc_dir=etc_dir)


def test_radio_native_only_ignores_fallback_marker(
    roots: tuple[Path, Path],
) -> None:
    """Radio is native-only: native once its binary is present, and a stray
    ``wfb-python-fallback`` marker is ignored (there is no packaged transmit
    plane to fall back to), so the aggregate stays native."""
    bin_dir, etc_dir = roots
    for b in _CORE:
        _make_bin(bin_dir, b)
    for b in ("ados-radio", "ados-net", "ados-plugin-host",
              "ados-display", "ados-display-probe"):
        _make_bin(bin_dir, b)

    # Radio native once its binary is present → drone native.
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "native"

    # A stray legacy fallback marker is ignored → still native.
    _touch_flag(etc_dir, "wfb-python-fallback")
    assert compute_runtime_mode("drone", bin_dir=bin_dir, etc_dir=etc_dir) == "native"


def test_radio_only_present_is_not_packaged(roots: tuple[Path, Path]) -> None:
    """A single flag-gated binary present (no core) still counts as
    'some native binary present' → not packaged."""
    bin_dir, etc_dir = roots
    _make_bin(bin_dir, "ados-radio")
    # No core binaries → hybrid (radio is native-only and native here since its
    # binary is present, but the core services are not native), and crucially
    # not 'packaged' since a binary exists.
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


def test_is_service_native_radio_native_only(roots: tuple[Path, Path]) -> None:
    """Radio is native-only (the packaged transmit plane was deleted): the
    per-service resolver the REST layer uses returns native once the binary is
    present, and a stray legacy fallback marker is ignored."""
    bin_dir, etc_dir = roots
    bin_dir.mkdir(parents=True, exist_ok=True)
    etc_dir.mkdir(parents=True, exist_ok=True)
    # No binary → not native (a broken install; the REST layer then falls back
    # to the demo manager, or 503s when none is present).
    assert not is_service_native("radio", bin_dir=bin_dir, etc_dir=etc_dir)
    # Binary present → native.
    _make_bin(bin_dir, "ados-radio")
    assert is_service_native("radio", bin_dir=bin_dir, etc_dir=etc_dir)
    # A stray legacy fallback marker is ignored — radio stays native.
    _touch_flag(etc_dir, "wfb-python-fallback")
    assert is_service_native("radio", bin_dir=bin_dir, etc_dir=etc_dir)


def test_is_service_native_hid_native_only(roots: tuple[Path, Path]) -> None:
    """Hid is native-only (the packaged PIC arbiter + input manager were
    deleted): native once its binaries are present, and a stray legacy
    hid-rust-enabled marker does not change the verdict."""
    bin_dir, etc_dir = roots
    bin_dir.mkdir(parents=True, exist_ok=True)
    etc_dir.mkdir(parents=True, exist_ok=True)
    _make_bin(bin_dir, "ados-pic")
    _make_bin(bin_dir, "ados-input")
    assert is_service_native("hid", bin_dir=bin_dir, etc_dir=etc_dir)
    # A stray legacy enable marker is ignored (there is no flag to honor).
    _touch_flag(etc_dir, "hid-rust-enabled")
    assert is_service_native("hid", bin_dir=bin_dir, etc_dir=etc_dir)


def test_is_service_native_net_native_only(roots: tuple[Path, Path]) -> None:
    """Net is native-only (the packaged uplink entrypoints were deleted): the
    per-service resolver the REST layer uses to decide whether to forward an
    operator radio knob to the native command socket returns native once the
    binary is present, and a stray legacy fallback marker is ignored."""
    bin_dir, etc_dir = roots
    bin_dir.mkdir(parents=True, exist_ok=True)
    etc_dir.mkdir(parents=True, exist_ok=True)
    # No binary → not native (the REST write paths then drive the in-process
    # managers directly rather than forwarding to the native socket).
    assert not is_service_native("net", bin_dir=bin_dir, etc_dir=etc_dir)
    # Binary present → native.
    _make_bin(bin_dir, "ados-net")
    assert is_service_native("net", bin_dir=bin_dir, etc_dir=etc_dir)
    # A stray legacy fallback marker is ignored — net stays native.
    _touch_flag(etc_dir, "net-python-fallback")
    assert is_service_native("net", bin_dir=bin_dir, etc_dir=etc_dir)


def test_is_service_native_unknown_is_false(roots: tuple[Path, Path]) -> None:
    """An unknown service name resolves to False, never raises."""
    bin_dir, etc_dir = roots
    assert not is_service_native("nope", bin_dir=bin_dir, etc_dir=etc_dir)


def test_runtime_mode_enrichment_shape() -> None:
    """The cloud-heartbeat sibling enricher returns the runtimeMode key."""
    from ados.services.cloud.heartbeat import build_runtime_mode_enrichment

    out = build_runtime_mode_enrichment(object())
    assert set(out) == {"runtimeMode"}
    assert out["runtimeMode"] in ("native", "hybrid", "packaged")
