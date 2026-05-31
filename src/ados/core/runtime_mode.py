"""Aggregate the agent's native-vs-packaged runtime mode for the wire.

The long-running services ship in two implementations behind a frozen
wire contract: a native compiled binary under ``/opt/ados/bin/ados-<svc>``
and the packaged Python service. Each service picks its branch at unit
start. A few core services run the native binary whenever it is present;
the rest are flag-gated and run native only when both a sentinel file
under ``/etc/ados`` is present and the binary is installed.

:func:`compute_runtime_mode` rolls those per-service branches up into one
of three labels the GCS surfaces as a node badge:

* ``"packaged"`` — no native binaries present at all (a pre-cutover,
  Python-only agent).
* ``"native"`` — every service applicable to the node's profile would
  run its native binary.
* ``"hybrid"`` — some native, some packaged.

The check is intentionally cheap (it only stats files, never shells out)
and total (a missing file resolves to packaged/python, it never raises),
so it is safe to call on the cloud-heartbeat hot path and from the
guaranteed-200 pairing-info endpoint.

The roots are parameters so the resolver can be exercised against a
temporary layout in tests; production callers use the real paths.
"""

from __future__ import annotations

import os
from pathlib import Path

# Native service binaries land here. Mirrors the ExecStart guards in
# data/systemd/*.service and the cutover flags owned by ``ados rust``.
_DEFAULT_BIN_DIR = Path("/opt/ados/bin")
_DEFAULT_ETC_DIR = Path("/etc/ados")

# Core services that run the native binary whenever it is present (no
# flag gate). The first three fail loud when the binary is missing; the
# router falls back to the packaged Python service. Either way the branch
# is "native iff the binary is on disk", so the same presence check
# decides the mode for all of them.
_CORE_BINARIES: tuple[str, ...] = (
    "ados-supervisor",
    "ados-video",
    "ados-cloud",
    "ados-mavlink-router",
)


class _FlagGated:
    """A flag-gated service: native only when both the flag and the
    binaries are present. ``profiles`` is the set of node profiles the
    service applies to (hyphen-form, matching the wire contract)."""

    __slots__ = ("flag", "binaries", "profiles")

    def __init__(
        self,
        flag: str,
        binaries: tuple[str, ...],
        profiles: tuple[str, ...],
    ) -> None:
        self.flag = flag
        self.binaries = binaries
        self.profiles = profiles


# Keyed by service. Flag names + binaries mirror the cutover table in
# ``ados.cli.rust``. ``profiles`` narrows each service to the nodes it
# can actually run on so a drone is not held back by a ground-station
# receive service it never starts (and vice versa). ``net``,
# ``plugin-host`` and ``display`` apply to both profiles.
_FLAG_GATED: dict[str, _FlagGated] = {
    "net": _FlagGated(
        flag="net-rust-enabled",
        binaries=("ados-net",),
        profiles=("drone", "ground-station"),
    ),
    "groundlink": _FlagGated(
        flag="groundlink-rust-enabled",
        binaries=("ados-groundlink",),
        profiles=("ground-station",),
    ),
    "plugin-host": _FlagGated(
        flag="plugin-host-rust-enabled",
        binaries=("ados-plugin-host",),
        profiles=("drone", "ground-station"),
    ),
    "hid": _FlagGated(
        flag="hid-rust-enabled",
        binaries=("ados-pic", "ados-input"),
        profiles=("ground-station",),
    ),
    "radio": _FlagGated(
        flag="wfb-rust-enabled",
        binaries=("ados-radio",),
        profiles=("drone",),
    ),
    "display": _FlagGated(
        flag="display-rust-enabled",
        binaries=("ados-display", "ados-display-probe"),
        profiles=("drone", "ground-station"),
    ),
}


def _bin_present(bin_dir: Path, name: str) -> bool:
    """True when the named binary exists and is executable. Total: any
    OS error (missing dir, permission) resolves to absent."""
    try:
        return os.access(bin_dir / name, os.X_OK)
    except OSError:
        return False


def _any_native_binary_present(bin_dir: Path) -> bool:
    """True when at least one native service binary is installed. The
    discriminator between a pre-cutover Python-only agent and one that
    has had any native binaries fetched."""
    core = any(_bin_present(bin_dir, b) for b in _CORE_BINARIES)
    if core:
        return True
    for svc in _FLAG_GATED.values():
        if any(_bin_present(bin_dir, b) for b in svc.binaries):
            return True
    return False


def compute_runtime_mode(
    profile: str = "drone",
    *,
    bin_dir: Path | None = None,
    etc_dir: Path | None = None,
) -> str:
    """Roll up the per-service native-vs-packaged branches into one label.

    ``profile`` is the node's wire-contract profile (``"drone"`` or
    ``"ground-station"``); it selects which flag-gated services count
    toward the aggregate. ``bin_dir`` / ``etc_dir`` default to the real
    install locations and are overridable for tests.

    Returns ``"packaged"`` when no native binaries are present anywhere,
    ``"native"`` when every applicable service would run its binary, and
    ``"hybrid"`` otherwise. Never raises.
    """
    bdir = bin_dir if bin_dir is not None else _DEFAULT_BIN_DIR
    edir = etc_dir if etc_dir is not None else _DEFAULT_ETC_DIR

    if not _any_native_binary_present(bdir):
        return "packaged"

    # Every applicable service contributes one native/not-native verdict.
    # Core services are native iff their binary is present.
    verdicts: list[bool] = [_bin_present(bdir, b) for b in _CORE_BINARIES]

    for svc in _FLAG_GATED.values():
        if profile not in svc.profiles:
            continue
        try:
            flag_set = (edir / svc.flag).exists()
        except OSError:
            flag_set = False
        binaries_present = all(_bin_present(bdir, b) for b in svc.binaries)
        verdicts.append(flag_set and binaries_present)

    return "native" if all(verdicts) else "hybrid"


__all__ = ["compute_runtime_mode"]
