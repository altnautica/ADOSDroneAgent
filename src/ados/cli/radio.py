"""``ados radio`` CLI subcommand tree.

Three operator-facing commands:

* ``ados radio status`` — pretty-print the current WFB-ng link.
* ``ados radio set-tx-power <DBM>`` — push a runtime TX power value.
* ``ados radio test`` — bench bring-up sweep across 1, 5, 10 dBm.

All three drive the agent's REST API at ``http://localhost:8080``;
nothing is run directly against the radio. The CLI is just a thin
client over the HTTP surface so behaviour is identical to what the
GCS sees.
"""

from __future__ import annotations

import json
import os
import platform
import shutil
import subprocess
from typing import Any

import click

from ados.cli._radio_base import (
    _format_int_or_dash,
    _format_rssi,
    _request,
    _rssi_color,
    radio_group,
)
from ados.cli._radio_pair import (  # noqa: F401 - re-exported at the old path
    _print_pair_status,
    pair_auto_group,
    pair_auto_off,
    pair_auto_on,
    pair_failover_status,
    pair_group,
    pair_local,
    pair_status,
    pair_unpair,
)
from ados.cli._radio_test import (  # noqa: F401 - re-exported at the old path
    _read_kernel_log_tail,
    radio_adapters,
    radio_test,
)

# The RTL8812EU (WFB) kernel module name and the vendored DKMS installer that the
# agent installer persists under the source tree (edge channel clones with
# submodules to /opt/ados/source; the OS steps read scripts/ + vendor/ there).
_RTL_MODULE = "8812eu"
_DRIVER_SCRIPT = "scripts/drivers/install-rtl8812eu.sh"
_SOURCE_DIRS = ("/opt/ados/source", "/opt/ados/repo")


def _rtl_module_present() -> bool:
    """True when the RTL8812EU kernel module is loaded or built on disk."""
    try:
        lsmod = subprocess.run(  # noqa: S603, S607
            ["lsmod"], capture_output=True, text=True, check=False
        )
        if lsmod.returncode == 0 and any(
            line.split(" ")[0] == _RTL_MODULE for line in lsmod.stdout.splitlines()
        ):
            return True
    except OSError:
        pass
    # Resolvable on disk even if not currently loaded.
    try:
        return (
            subprocess.run(  # noqa: S603, S607
                ["modinfo", _RTL_MODULE], capture_output=True, check=False
            ).returncode
            == 0
        )
    except OSError:
        return False


def _resolve_driver_script() -> str | None:
    """The DKMS installer path under the persisted source tree, or None."""
    roots: list[str] = []
    env_dir = os.environ.get("ADOS_SOURCE_DIR")
    if env_dir:
        roots.append(env_dir)
    roots.extend(_SOURCE_DIRS)
    for root in roots:
        candidate = os.path.join(root, _DRIVER_SCRIPT)
        if os.path.isfile(candidate):
            return candidate
    return None


_BIND_TERMINAL_STATES = {"idle", "paired", "failed", "aborted"}


def _bind_session_state(bind: dict[str, Any] | None) -> str | None:
    """Pull the bind-session state from the /api/wfb/pair/local-bind snapshot,
    tolerating either a flat session dict or one nested under 'session'."""
    if not isinstance(bind, dict):
        return None
    sess = bind.get("session") if isinstance(bind.get("session"), dict) else bind
    state = sess.get("state")
    return state if isinstance(state, str) else None


def _print_status_table(data: dict[str, Any], bind: dict[str, Any] | None = None) -> None:
    """Pretty-print /api/wfb status to the terminal."""
    state = str(data.get("state") or "unknown")
    iface = data.get("interface") or "—"
    adapter = data.get("adapter") or {}
    driver = adapter.get("driver") or "—"
    chipset = adapter.get("chipset") or "—"
    channel = data.get("actual_channel") or 0
    freq = data.get("frequency_mhz") or 0
    rssi = data.get("rssi_dbm")
    if rssi == -100.0:
        rssi = None
    bitrate = data.get("bitrate_kbps") or 0
    bitrate_mbps = (bitrate / 1000.0) if bitrate else None
    tx = data.get("tx_power_dbm")
    tx_max = data.get("tx_power_max_dbm")
    topology = data.get("topology") or "—"
    fec_r = data.get("fec_recovered")
    fec_l = data.get("fec_failed")
    # Selected radio adapter identity + injection verdict. The selector
    # only ever picks an iface whose monitor mode was proven, so a False
    # verdict means the scan RAN and no RTL injection-capable adapter was
    # found — the link is stranded and we must NOT be transmitting on a
    # management WiFi. The verdict is three-state: a null means no scan has
    # produced one yet (unpaired / pre-scan), which must not render the
    # alarming measured-red line about a scan that never happened.
    selected_chipset = data.get("adapter_chipset")
    injection_verdict = data.get("adapter_injection_ok")
    # A bind session that is mid-flight (any non-terminal state) tears the
    # radio down and rebuilds it, so a transient 'no injection radio' verdict
    # is expected and not an error — show the bind phase instead of the
    # alarming red line.
    bind_state = _bind_session_state(bind)
    bind_active = bool(bind_state) and bind_state not in _BIND_TERMINAL_STATES
    if injection_verdict is True:
        injection_value = "yes"
        injection_colour = "green"
    elif bind_active:
        injection_value = f"binding ({bind_state}) — connecting"
        injection_colour = "yellow"
    elif injection_verdict is False:
        injection_value = "NO — no injection radio"
        injection_colour = "red"
    else:
        injection_value = "unknown — no adapter scan yet"
        injection_colour = "yellow"

    click.echo(click.style(f"WFB-ng radio  state={state}", bold=True))
    click.echo("")

    rows: list[tuple[str, str, str | None]] = [
        ("Interface", str(iface), None),
        ("Driver / chipset", f"{driver} / {chipset}", None),
        ("Selected radio", str(selected_chipset or "—"), None),
        ("Injection capable", injection_value, injection_colour),
        (
            "Channel / freq",
            f"{channel}  ({freq} MHz)" if channel else "—",
            None,
        ),
        ("RSSI", _format_rssi(rssi), _rssi_color(rssi)),
        (
            "Bitrate",
            f"{bitrate_mbps:.2f} Mbps" if bitrate_mbps else "—",
            None,
        ),
        (
            "TX power",
            (
                f"{tx} / {tx_max} dBm"
                if tx is not None and tx_max is not None
                else "—"
            ),
            None,
        ),
        ("Topology", str(topology), None),
        (
            "FEC R / L",
            f"{_format_int_or_dash(fec_r)} / {_format_int_or_dash(fec_l)}",
            None,
        ),
    ]

    label_width = max(len(label) for label, _, _ in rows)
    for label, value, colour in rows:
        line = f"  {label:<{label_width}}  {value}"
        if colour:
            click.echo(click.style(line, fg=colour))
        else:
            click.echo(line)


@radio_group.command("status", help="Show the current WFB-ng link status.")
@click.option("--json", "as_json", is_flag=True, help="Machine-readable output.")
def radio_status(as_json: bool) -> None:
    _, data = _request("GET", "/api/wfb")
    # Also pull the bind-session snapshot so a transient 'no injection radio'
    # verdict during an active bind is not rendered as a red error. Best-effort:
    # any failure leaves bind empty and the data-plane view is shown as before.
    bind: dict[str, Any] = {}
    try:
        _, bind = _request("GET", "/api/wfb/pair/local-bind")
    except Exception:
        bind = {}
    if not isinstance(bind, dict):
        bind = {}
    if as_json:
        click.echo(json.dumps({"radio": data, "bind": bind}, indent=2, sort_keys=True))
    else:
        _print_status_table(data, bind)
    bind_state = _bind_session_state(bind)
    bind_active = bool(bind_state) and bind_state not in _BIND_TERMINAL_STATES
    if (data.get("state") in ("absent", "disabled")) and not bind_active:
        raise click.exceptions.Exit(code=1)


@radio_group.command(
    "set-tx-power",
    help="Set the WFB-ng TX power in dBm. Refused above the configured ceiling.",
)
@click.argument("dbm", type=int)
def radio_set_tx_power(dbm: int) -> None:
    status, body = _request(
        "PUT",
        "/api/wfb/tx-power",
        json={"tx_power_dbm": dbm},
        raise_for_status=False,
    )
    if status == 400:
        detail = body.get("detail") if isinstance(body, dict) else None
        if isinstance(detail, dict):
            err = detail.get("error")
            if err == "above_ceiling":
                ceiling = detail.get("max")
                click.echo(
                    click.style(
                        f"Refused: {dbm} dBm exceeds the configured "
                        f"ceiling of {ceiling} dBm.",
                        fg="red",
                    )
                )
                raise click.exceptions.Exit(code=1)
            if err == "below_floor":
                floor = detail.get("min")
                click.echo(
                    click.style(
                        f"Refused: {dbm} dBm is below the configured "
                        f"floor of {floor} dBm.",
                        fg="red",
                    )
                )
                raise click.exceptions.Exit(code=1)
        click.echo(click.style(f"Refused: {detail or body}", fg="red"))
        raise click.exceptions.Exit(code=1)
    if status == 503:
        click.echo(
            click.style(
                "WFB-ng service not running. Start the supervisor first.",
                fg="red",
            )
        )
        raise click.exceptions.Exit(code=1)
    if status >= 400:
        click.echo(click.style(f"Refused: HTTP {status}: {body}", fg="red"))
        raise click.exceptions.Exit(code=1)

    requested = body.get("requested_dbm")
    effective = body.get("effective_dbm")
    eff_str = "n/a" if effective is None else f"{effective}"
    click.echo(f"Set TX power: requested={requested} effective={eff_str}")


@radio_group.command(
    "hop",
    help="Coordinated hop to a WFB channel (the GS follows via the announce).",
)
@click.argument("channel", type=int)
def radio_hop(channel: int) -> None:
    """Request a coordinated channel hop to CHANNEL.

    Drives ``POST /api/wfb/channel``, which forwards to the native radio's
    command socket so the announce + dwell-sync brings the ground station with
    it. A refused hop (not paired / no peer / mid-bind / invalid channel)
    reports the reason and exits non-zero.
    """
    status, body = _request(
        "POST",
        "/api/wfb/channel",
        json={"channel": channel},
        raise_for_status=False,
    )
    if status == 400:
        detail = body.get("detail") if isinstance(body, dict) else None
        click.echo(click.style(f"Refused: {detail or body}", fg="red"))
        raise click.exceptions.Exit(code=1)
    if status == 409:
        detail = body.get("detail") if isinstance(body, dict) else None
        reason = detail.get("message") if isinstance(detail, dict) else (detail or body)
        click.echo(click.style(f"Hop refused: {reason}", fg="yellow"))
        raise click.exceptions.Exit(code=2)
    if status == 503:
        click.echo(
            click.style(
                "WFB-ng service not running. Start the supervisor first.",
                fg="red",
            )
        )
        raise click.exceptions.Exit(code=1)
    if status >= 400:
        click.echo(click.style(f"Hop failed: HTTP {status}: {body}", fg="red"))
        raise click.exceptions.Exit(code=1)

    # 202: the radio validated and started the hop; it commits only once the
    # ground station acks the announce, so this is the target, not the result.
    ch = body.get("target_channel")
    freq = body.get("frequency_mhz")
    click.echo(
        click.style(f"Hop initiated → channel {ch} ({freq} MHz).", fg="green")
    )


@radio_group.command(
    "install-driver",
    help="Build and install the RTL8812EU (WFB) kernel driver if it is missing.",
)
@click.option("--force", is_flag=True, help="Rebuild even if the module is present.")
def radio_install_driver(force: bool) -> None:
    """Install the vendored RTL8812EU DKMS driver.

    Runs the idempotent driver script from the persisted source tree. The script
    needs root, so it is run under sudo when the CLI is not already root. When no
    source tree is present (a wheel-only install), the operator is pointed at
    ``ados update``, which refetches the source and builds the driver.
    """
    if platform.system() != "Linux":
        click.echo("The RTL8812EU driver only applies on a Linux SBC.")
        raise click.exceptions.Exit(code=1)

    if not force and _rtl_module_present():
        click.echo("RTL8812EU driver already installed. Use --force to rebuild.")
        return

    script = _resolve_driver_script()
    if script is None:
        click.echo(
            "Driver source not found on this device. Run `ados update` to fetch "
            "the source and build the driver."
        )
        raise click.exceptions.Exit(code=1)

    argv = ["bash", script]
    if os.geteuid() != 0:
        if shutil.which("sudo") is None:
            click.echo(
                "Installing the driver needs root. Re-run as: "
                "sudo ados radio install-driver"
            )
            raise click.exceptions.Exit(code=1)
        argv = ["sudo", *argv]

    try:
        completed = subprocess.run(argv, check=False)  # noqa: S603
    except OSError as exc:
        raise click.ClickException(f"Failed to run the driver installer: {exc}") from exc
    if completed.returncode != 0:
        raise click.ClickException(
            f"Driver install finished with exit code {completed.returncode}."
        )
    click.echo("RTL8812EU driver installed.")
