"""``ados rust`` CLI subcommand tree.

Operator-facing control over the native-vs-packaged cutover for the
long-running services that ship in two implementations behind a frozen
wire contract. Each such service runs the native binary only when BOTH a
flag file under ``/etc/ados`` is present AND the binary is installed;
otherwise the packaged Python service is the default. This command owns
that flag file (and the surrounding systemd reconcile) so the choice is
made through the agent's own tooling and stays reproducible from the
install, instead of an out-of-band edit on the box.

* ``ados rust status`` — show, per service, the flag, the binary, and
  the live unit state.
* ``ados rust enable <svc>...`` — turn the native implementation on.
* ``ados rust disable <svc>...`` — fall back to the packaged service.

``enable`` / ``disable`` touch ``/etc/ados`` and drive ``systemctl``, so
they require root. ``status`` is read-only and runs as any user.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from dataclasses import dataclass

import click

from ados.core.paths import ADOS_ETC_DIR


@dataclass(frozen=True)
class _Service:
    """One cutover-capable service.

    ``flag`` is the sentinel file under ``/etc/ados`` the unit's ExecStart
    checks. ``binaries`` are the native artifacts that must be present for
    the native branch to actually run. ``swap_units`` carry both
    implementations and are restarted in place to switch. ``extra_units``
    exist only for the native path (they are off by default) and are
    enabled on and disabled off. ``subsumes`` are packaged units the
    native daemon absorbs in-process and that must be masked while it runs.

    ``opt_out`` flips the sense of ``flag``. With ``opt_out=False`` (the
    default), the flag is an opt-IN: the unit runs native only when the
    flag is present. With ``opt_out=True`` the service has already cut over
    — native is the DEFAULT when the binaries are present, and the flag is
    a ``*-fallback`` marker that pins the packaged path. ``enable`` then
    removes the marker and ``disable`` writes it.
    """

    flag: str
    binaries: tuple[str, ...]
    swap_units: tuple[str, ...] = ()
    extra_units: tuple[str, ...] = ()
    subsumes: tuple[str, ...] = ()
    note: str = ""
    opt_out: bool = False


# Keyed by the operator-facing service name. Mirrors the ExecStart guards
# in data/systemd/*.service and the reconcile in the installer
# (crates/ados-installer/src/steps/systemd.rs). The WFB transmit and
# ground-receive planes are native-only (their packaged predecessor was
# deleted) and so are not listed here — there is nothing to fall back to.
_SERVICES: dict[str, _Service] = {
    "net": _Service(
        flag="net-rust-enabled",
        binaries=("/opt/ados/bin/ados-net",),
        swap_units=("ados-uplink-router",),
        subsumes=(
            "ados-ethernet",
            "ados-wifi-client",
            "ados-usb-gadget",
        ),
        note="native uplink matrix; absorbs ethernet/wifi-client/usb-gadget",
    ),
    "plugin-host": _Service(
        # Cut over: native is the default. The unit's ExecStart runs the
        # native binary unless the fallback marker pins the packaged host
        # server (served by the supervisor). `enable` removes the marker,
        # `disable` writes it. The two share the per-plugin sockets, so
        # exactly one is the active owner.
        flag="plugin-host-python-fallback",
        binaries=("/opt/ados/bin/ados-plugin-host",),
        swap_units=("ados-plugin-host",),
        note="native plugin host (default); the fallback marker pins the packaged path",
        opt_out=True,
    ),
    "hid": _Service(
        flag="hid-rust-enabled",
        binaries=("/opt/ados/bin/ados-pic", "/opt/ados/bin/ados-input"),
        extra_units=("ados-pic", "ados-input"),
        subsumes=("ados-buttons",),
        note="native input arbiter; absorbs the button service",
    ),
    "display": _Service(
        flag="display-python-fallback",
        binaries=(
            "/opt/ados/bin/ados-display",
            "/opt/ados/bin/ados-display-probe",
        ),
        swap_units=("ados-oled", "ados-display-probe"),
        note="native display (default); the fallback marker pins the packaged UI",
        opt_out=True,
    ),
    "logd": _Service(
        flag="logd-python-fallback",
        binaries=("/opt/ados/bin/ados-logd",),
        # The store is on by default: the log-view endpoints read it, so a fresh
        # install brings it up. It is native-only with no packaged counterpart,
        # so it is an extra unit (enabled + started on), not a swap. The
        # fallback marker pins it OFF — ``disable`` writes the marker and stops
        # the unit, ``enable`` removes it and starts the unit again.
        extra_units=("ados-logd",),
        note="local logging and telemetry store (default; fallback marker pins it off)",
        opt_out=True,
    ),
    "control": _Service(
        # Opt-in, off by default: the GCS uses the FastAPI surface (:8080), and
        # the native control surface is a net-new dual-listener (LAN :8082 +
        # /run/ados/control.sock) with no packaged counterpart, so it is an extra
        # unit enabled+started on and masked off. `enable` writes the marker and
        # starts it; `disable` removes the marker and masks the unit.
        flag="control-rust-enabled",
        binaries=("/opt/ados/bin/ados-control",),
        extra_units=("ados-control",),
        note="native HTTP control surface (LAN :8082 + /run/ados/control.sock); off by default",
    ),
}

_SVC_NAMES = tuple(_SERVICES)


def _require_root() -> None:
    if os.geteuid() != 0:
        raise click.ClickException(
            "This command writes /etc/ados and restarts services; run it with sudo."
        )


def _systemctl(*args: str, timeout: float = 60.0) -> int:
    """Best-effort systemctl call. Returns the exit code (124 on timeout);
    never raises."""
    if not shutil.which("systemctl"):
        return 0
    try:
        result = subprocess.run(
            ["systemctl", *args],
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        return result.returncode
    except subprocess.TimeoutExpired:
        return 124
    except OSError as exc:
        click.echo(click.style(f"  warn: systemctl {' '.join(args)}: {exc}", fg="yellow"))
        return 1


def _mask_unit(unit: str) -> None:
    """Stop, SIGKILL if stubborn, disable, then clear any failed state.

    A packaged manager that is slow to honor SIGTERM would otherwise blow
    past the stop timeout and get SIGKILLed by systemd into the ``failed``
    state *after* a bare ``reset-failed`` already ran, leaving a disabled
    unit lingering as failed in ``systemctl --failed``. Escalating to
    SIGKILL before the disable + reset-failed makes the unit end cleanly
    inactive.
    """
    if _systemctl("stop", unit, timeout=15.0) == 124:
        _systemctl("kill", "-s", "SIGKILL", unit, timeout=10.0)
        _systemctl("stop", unit, timeout=10.0)
    _systemctl("disable", unit)
    _systemctl("reset-failed", unit)


def _flag_path(svc: _Service):
    return ADOS_ETC_DIR / svc.flag


def _binaries_present(svc: _Service) -> bool:
    return all(os.access(b, os.X_OK) for b in svc.binaries)


def _unit_active(unit: str) -> bool:
    return _systemctl("is-active", "--quiet", unit) == 0


def _flag_active(svc: _Service) -> bool:
    """True when the native branch is selected by the flag, accounting for
    the flag's sense. Opt-in services need the flag present; opt-out
    services (already cut over) need the fallback marker absent."""
    present = _flag_path(svc).exists()
    return (not present) if svc.opt_out else present


def _mode(svc: _Service) -> str:
    """The branch the unit's ExecStart would take right now. The native
    binary runs only when the flag selects it (opt-in: flag present;
    opt-out: fallback marker absent) AND the binaries are installed."""
    if not _flag_active(svc):
        return "python"
    return "rust" if _binaries_present(svc) else "python (binary missing)"


@click.group("rust", help="Switch services between the native and packaged implementations.")
def rust_group() -> None:
    pass


@rust_group.command("status", help="Show the native-vs-packaged state per service.")
def rust_status() -> None:
    name_w = max(len(n) for n in _SVC_NAMES)
    click.echo(
        click.style(f"  {'service':<{name_w}}  {'mode':<22}  flag      binary   units", bold=True)
    )
    for name in _SVC_NAMES:
        svc = _SERVICES[name]
        mode = _mode(svc)
        # For opt-in services the flag presence == native-selected; for
        # opt-out services the marker presence == packaged-pinned. Show the
        # marker state in both cases (a present opt-out marker reads as
        # "fallback").
        if _flag_path(svc).exists():
            flag = "fallback" if svc.opt_out else "set"
        else:
            flag = "—"
        binp = "present" if _binaries_present(svc) else "absent"
        watched = svc.swap_units + svc.extra_units
        active = [u for u in watched if _unit_active(u)]
        units = ",".join(u.removeprefix("ados-") for u in active) or "—"
        colour = "green" if mode == "rust" else None
        line = f"  {name:<{name_w}}  {mode:<22}  {flag:<8}  {binp:<7}  {units}"
        click.echo(click.style(line, fg=colour) if colour else line)


def _set_marker(svc: _Service, *, native: bool) -> None:
    """Write or remove the sentinel so the next ExecStart picks ``native``.

    For an opt-in service the marker selects native, so native => write,
    packaged => remove. For an opt-out service (already cut over) the
    marker is the packaged-fallback pin, so the sense is inverted: native
    => remove the marker, packaged => write it.
    """
    path = _flag_path(svc)
    want_marker = (not native) if svc.opt_out else native
    if want_marker:
        ADOS_ETC_DIR.mkdir(parents=True, exist_ok=True)
        path.touch()
    elif path.exists():
        path.unlink()


def _apply(svc: _Service, *, enable: bool) -> None:
    if enable:
        _set_marker(svc, native=True)
        # Mask the packaged units the native daemon absorbs so they do not
        # fight it for the same device or socket.
        for unit in svc.subsumes:
            _mask_unit(unit)
        # Swap units carry both implementations: a restart re-execs the
        # native branch. Extra units exist only for the native path.
        for unit in svc.swap_units:
            _systemctl("restart", unit)
        for unit in svc.extra_units:
            _systemctl("enable", unit)
            _systemctl("restart", unit)
    else:
        _set_marker(svc, native=False)
        # Retire the native-only units, then bring the packaged ones back.
        for unit in svc.extra_units:
            _mask_unit(unit)
        for unit in svc.subsumes:
            _systemctl("enable", unit)
            _systemctl("restart", unit)
        for unit in svc.swap_units:
            _systemctl("restart", unit)


@rust_group.command("enable", help="Run the native implementation for one or more services.")
@click.argument("services", nargs=-1, required=True, type=click.Choice(_SVC_NAMES))
def rust_enable(services: tuple[str, ...]) -> None:
    _require_root()
    for name in services:
        svc = _SERVICES[name]
        if not _binaries_present(svc):
            click.echo(
                click.style(
                    f"  {name}: native binary not installed — run install.sh --upgrade first.",
                    fg="yellow",
                )
            )
            continue
        _apply(svc, enable=True)
        click.echo(click.style(f"  {name}: native implementation enabled.", fg="green"))
    rust_status.callback()  # type: ignore[misc]


@rust_group.command("disable", help="Fall back to the packaged service for one or more services.")
@click.argument("services", nargs=-1, required=True, type=click.Choice(_SVC_NAMES))
def rust_disable(services: tuple[str, ...]) -> None:
    _require_root()
    for name in services:
        _apply(_SERVICES[name], enable=False)
        click.echo(f"  {name}: reverted to the packaged service.")
    rust_status.callback()  # type: ignore[misc]
