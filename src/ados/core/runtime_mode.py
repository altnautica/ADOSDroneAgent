"""Aggregate the agent's native-vs-packaged runtime mode for the wire.

The long-running services ship in two implementations behind a frozen
wire contract: a native compiled binary under ``/opt/ados/bin/ados-<svc>``
and the packaged Python service. Each service picks its branch at unit
start. Core services (and those whose packaged predecessor has been
deleted) run the native binary whenever it is present; the rest are
flag-gated and run native only when both a sentinel file under
``/etc/ados`` is present and the binary is installed.

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
# flag gate). "native iff the binary is on disk", so the same presence
# check decides the mode for all of them. These are profile-scoped to match
# the installer's per-profile binary catalog (crates/ados-installer/binaries.rs):
# counting a binary the installer never fetches for this profile would pin the
# aggregate at "hybrid" forever (ados-video is drone-only; a ground station that
# is otherwise fully native would never report "native").
_CORE_BINARIES_UNIVERSAL: tuple[str, ...] = (
    "ados-supervisor",
    "ados-cloud",
    "ados-mavlink-router",
)
# Drone-only core (the air-side encode pipeline; a ground station receives
# video through ados-mediamtx-gs, never ados-video).
_CORE_BINARIES_DRONE: tuple[str, ...] = ("ados-video",)


def _core_binaries_for(profile: str) -> tuple[str, ...]:
    """The core binaries the installer fetches for ``profile``."""
    if profile == "drone":
        return _CORE_BINARIES_UNIVERSAL + _CORE_BINARIES_DRONE
    return _CORE_BINARIES_UNIVERSAL


# Every core binary across all profiles — used only by the
# "is ANY native binary present" pre-cutover discriminator.
_CORE_BINARIES: tuple[str, ...] = _CORE_BINARIES_UNIVERSAL + _CORE_BINARIES_DRONE


class _FlagGated:
    """A service whose native-vs-packaged branch turns on a sentinel file
    under ``/etc/ados`` — or, once the packaged predecessor is deleted, a
    native-only service with no fallback to gate.

    Senses, set by ``flag`` and ``opt_out``:

    * ``flag=None`` — native-ONLY: the packaged implementation was deleted,
      so the service runs its native binary whenever the binary is present
      (any stray ``*-fallback`` marker is ignored). The unit ExecStart runs
      the binary directly.
    * ``opt_out=False`` (default) — opt-IN: native only when BOTH the flag
      file and the binaries are present (the routine, pre-cutover posture).
    * ``opt_out=True`` — opt-OUT: native is the DEFAULT when the binaries
      are present; the flag file (a ``*-fallback`` marker) pins the
      packaged path instead. This mirrors a unit whose ExecStart has
      already cut over to the native binary by default.

    ``profiles`` is the set of node profiles the service applies to
    (hyphen-form, matching the wire contract)."""

    __slots__ = ("flag", "binaries", "profiles", "opt_out")

    def __init__(
        self,
        flag: str | None,
        binaries: tuple[str, ...],
        profiles: tuple[str, ...],
        opt_out: bool = False,
    ) -> None:
        self.flag = flag
        self.binaries = binaries
        self.profiles = profiles
        self.opt_out = opt_out


# Keyed by service. Flag names + binaries mirror the cutover table in
# ``ados.cli.rust``. ``profiles`` narrows each service to the nodes it
# can actually run on so a drone is not held back by a ground-station
# receive service it never starts (and vice versa). ``net``,
# ``plugin-host`` and ``display`` apply to both profiles.
_FLAG_GATED: dict[str, _FlagGated] = {
    "net": _FlagGated(
        # Native-only: the packaged uplink-router + manager entrypoints were
        # deleted, so there is no Python fallback to toggle. The native ados-net
        # daemon always runs; the REST WiFi write paths forward to its command
        # socket. The manager classes stay (the REST read paths use them).
        # Ground-station only — the installer fetches ados-net for GROUND only
        # and the drone profile masks ados-uplink-router.service, so counting it
        # on a drone (where the binary is never installed) would pin the drone's
        # aggregate at "hybrid".
        flag=None,
        binaries=("ados-net",),
        profiles=("ground-station",),
    ),
    "groundlink": _FlagGated(
        # Native-only on every role: the packaged direct-role receive plane and
        # the mesh relay/receiver modules were all deleted, so there is no
        # Python fallback to toggle.
        flag=None,
        binaries=("ados-groundlink",),
        profiles=("ground-station",),
    ),
    "plugin-host": _FlagGated(
        # Cut over: native is the default. The host runs its native binary
        # whenever the binary is present; the plugin-host-python-fallback
        # marker pins the packaged host server instead. The two are mutually
        # exclusive (both bind /run/ados/plugins/<id>.sock), so exactly one
        # owns the per-plugin sockets at a time.
        flag="plugin-host-python-fallback",
        binaries=("ados-plugin-host",),
        profiles=("drone", "ground-station"),
        opt_out=True,
    ),
    "hid": _FlagGated(
        flag="hid-rust-enabled",
        binaries=("ados-pic", "ados-input"),
        profiles=("ground-station",),
    ),
    "radio": _FlagGated(
        # Native-only: the packaged transmit plane was deleted.
        flag=None,
        binaries=("ados-radio",),
        profiles=("drone",),
    ),
    "display": _FlagGated(
        flag="display-python-fallback",
        binaries=("ados-display", "ados-display-probe"),
        profiles=("drone", "ground-station"),
        opt_out=True,
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


def is_service_native(
    service: str,
    *,
    bin_dir: Path | None = None,
    etc_dir: Path | None = None,
) -> bool:
    """True when the named flag-gated service would run its native binary.

    Resolves one service's native-vs-packaged branch with the SAME rule the
    aggregate :func:`compute_runtime_mode` applies: an opt-out service is
    native when its binaries are present and the operator has NOT pinned the
    packaged fallback marker; an opt-in service is native only when both the
    flag and the binaries are present. Unknown services resolve to ``False``.

    The check is cheap (it only stats files) and total (it never raises), so
    a REST handler can call it on the request path to decide whether to
    forward an operator radio knob to the native command socket or to drive
    the packaged manager in-process.
    """
    bdir = bin_dir if bin_dir is not None else _DEFAULT_BIN_DIR
    edir = etc_dir if etc_dir is not None else _DEFAULT_ETC_DIR
    svc = _FLAG_GATED.get(service)
    if svc is None:
        return False
    binaries_present = all(_bin_present(bdir, b) for b in svc.binaries)
    if not binaries_present:
        return False
    if svc.flag is None:
        # Native-only: no packaged predecessor to fall back to, so the
        # binary being present is sufficient (any stray marker is ignored).
        return True
    try:
        flag_set = (edir / svc.flag).exists()
    except OSError:
        flag_set = False
    # opt_out: native is the default; the fallback marker pins packaged.
    # opt_in: native only when the flag is also present.
    return not flag_set if svc.opt_out else flag_set


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
    # Core services are native iff their binary is present — scoped to the
    # binaries the installer actually fetches for this profile (a binary it
    # never fetches must not count as a non-native verdict).
    verdicts: list[bool] = [_bin_present(bdir, b) for b in _core_binaries_for(profile)]

    for svc in _FLAG_GATED.values():
        if profile not in svc.profiles:
            continue
        binaries_present = all(_bin_present(bdir, b) for b in svc.binaries)
        if svc.flag is None:
            # Native-only service: native iff the binary is present.
            verdicts.append(binaries_present)
            continue
        try:
            flag_set = (edir / svc.flag).exists()
        except OSError:
            flag_set = False
        if svc.opt_out:
            # Default-native: native when the binaries are present and the
            # operator has NOT pinned the packaged fallback marker.
            verdicts.append(binaries_present and not flag_set)
        else:
            # Opt-in: native only when both the flag and the binaries are set.
            verdicts.append(flag_set and binaries_present)

    return "native" if all(verdicts) else "hybrid"


__all__ = ["compute_runtime_mode", "is_service_native"]
