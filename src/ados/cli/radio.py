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
import time
from typing import Any

import click
import httpx

from ados.core.paths import PAIRING_JSON

API_BASE = "http://localhost:8080"

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


def _load_api_key() -> str | None:
    try:
        if PAIRING_JSON.exists():
            data = json.loads(PAIRING_JSON.read_text(encoding="utf-8"))
            key = data.get("api_key")
            return key if isinstance(key, str) else None
    except (OSError, ValueError, json.JSONDecodeError):
        pass
    return None


def _auth_headers() -> dict[str, str]:
    key = _load_api_key()
    return {"X-ADOS-Key": key} if key else {}


def _request(
    method: str,
    path: str,
    *,
    raise_for_status: bool = True,
    **kwargs: Any,
) -> tuple[int, dict[str, Any]]:
    """Minimal REST helper. Returns (status_code, parsed_body)."""
    try:
        with httpx.Client(timeout=kwargs.pop("timeout", 8.0)) as client:
            resp = client.request(
                method,
                f"{API_BASE}{path}",
                headers=_auth_headers(),
                **kwargs,
            )
            try:
                body = resp.json()
            except ValueError:
                body = {"text": resp.text}
            if raise_for_status and resp.status_code >= 400:
                detail = body.get("detail") if isinstance(body, dict) else None
                raise click.ClickException(
                    f"Agent API returned {resp.status_code}: {detail or body}"
                )
            return resp.status_code, body if isinstance(body, dict) else {"data": body}
    except httpx.ConnectError as exc:
        raise click.ClickException(
            "Agent is not running on http://localhost:8080. "
            "Start the supervisor or run `ados demo`."
        ) from exc
    except httpx.HTTPError as exc:
        raise click.ClickException(str(exc)) from exc


def _rssi_color(rssi: float | None) -> str:
    """Map RSSI dBm to a plain colour name. None and weak signals are red."""
    if rssi is None:
        return "red"
    if rssi >= -65.0:
        return "green"
    if rssi >= -78.0:
        return "yellow"
    return "red"


def _format_rssi(rssi: float | None) -> str:
    if rssi is None:
        return "n/a"
    return f"{rssi:.1f} dBm"


def _format_int_or_dash(value: Any) -> str:
    if value is None or value == 0:
        return "—" if value is None else "0"
    return str(value)


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
    channel = data.get("channel") or 0
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
    # only ever picks an iface whose monitor mode was proven, so a false
    # verdict means no RTL injection-capable adapter was found — the link
    # is stranded and we must NOT be transmitting on a management WiFi.
    selected_chipset = data.get("adapter_chipset")
    injection_ok = bool(data.get("adapter_injection_ok", False))
    # A bind session that is mid-flight (any non-terminal state) tears the
    # radio down and rebuilds it, so a transient 'no injection radio' verdict
    # is expected and not an error — show the bind phase instead of the
    # alarming red line.
    bind_state = _bind_session_state(bind)
    bind_active = bool(bind_state) and bind_state not in _BIND_TERMINAL_STATES
    if injection_ok:
        injection_value = "yes"
        injection_colour = "green"
    elif bind_active:
        injection_value = f"binding ({bind_state}) — connecting"
        injection_colour = "yellow"
    else:
        injection_value = "NO — no injection radio"
        injection_colour = "red"

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


@click.group("radio", help="Inspect and tune the WFB-ng radio link.")
def radio_group() -> None:
    pass


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

    ch = body.get("channel")
    freq = body.get("frequency_mhz")
    click.echo(
        click.style(f"Hop initiated → channel {ch} ({freq} MHz).", fg="green")
    )


def _read_kernel_log_tail() -> list[str]:
    """Best-effort grab the last 10 kernel log lines for the test sweep."""
    try:
        result = subprocess.run(
            ["journalctl", "-k", "-n", "10", "--no-pager"],
            capture_output=True,
            text=True,
            timeout=3,
        )
        if result.returncode == 0:
            return [line for line in result.stdout.splitlines() if line.strip()]
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        pass
    return []


@radio_group.command(
    "test",
    help="Bench bring-up sweep: walk through 1, 5, 10 dBm and report effect.",
)
def radio_test() -> None:
    _, status_data = _request("GET", "/api/wfb")
    state = status_data.get("state")
    if state == "absent" or state == "disabled":
        click.echo(
            click.style(
                "No radio detected — plug in an RTL8812EU first.",
                fg="red",
            )
        )
        raise click.exceptions.Exit(code=1)

    original_tx = status_data.get("tx_power_dbm")
    if original_tx is None:
        original_tx = 5

    click.echo(
        click.style(
            f"Starting TX-power sweep. Will restore tx_power_dbm={original_tx} at the end.",
            bold=True,
        )
    )

    sweep = [1, 5, 10]
    for dbm in sweep:
        click.echo("")
        click.echo(click.style(f"→ Setting TX power to {dbm} dBm", fg="cyan"))
        code, body = _request(
            "PUT",
            "/api/wfb/tx-power",
            json={"tx_power_dbm": dbm},
            raise_for_status=False,
        )
        if code >= 400:
            click.echo(
                click.style(f"  Refused: HTTP {code}: {body}", fg="red")
            )
            continue

        time.sleep(3.0)

        _, post_status = _request("GET", "/api/wfb")
        rssi = post_status.get("rssi_dbm")
        if rssi == -100.0:
            rssi = None
        click.echo(
            f"  effective={body.get('effective_dbm')} dBm   "
            f"RSSI={_format_rssi(rssi)}"
        )

        kernel_tail = _read_kernel_log_tail()
        flagged = [line for line in kernel_tail if "undervoltage" in line.lower()]
        if flagged:
            click.echo(click.style("  Kernel undervoltage warnings:", fg="yellow"))
            for line in flagged:
                click.echo(f"    {line}")

    click.echo("")
    click.echo(click.style(f"Restoring TX power to {original_tx} dBm", fg="cyan"))
    _request(
        "PUT",
        "/api/wfb/tx-power",
        json={"tx_power_dbm": int(original_tx)},
        raise_for_status=False,
    )
    click.echo("Done.")


@radio_group.command(
    "adapters",
    help="List detected WiFi adapters and their WFB injection verdict.",
)
@click.option("--json", "as_json", is_flag=True, help="Machine-readable output.")
def radio_adapters(as_json: bool) -> None:
    """List the detected WiFi adapters and which are WFB-injection capable.

    Resolves the live adapter facts through the radio service's stats
    sidecar / one-shot scan, falling back to a local ``iw`` scan when the
    radio is not running. Reads facts directly (not over REST) so it
    works before the agent API is up.
    """
    from ados.services.wfb.adapter_probe import detect_wfb_adapters

    adapters = detect_wfb_adapters()

    if as_json:
        payload = [
            {
                "interface_name": a.interface_name,
                "driver": a.driver,
                "chipset": a.chipset,
                "supports_monitor": a.supports_monitor,
                "current_mode": a.current_mode,
                "phy": a.phy,
                "usb_vid": a.usb_vid,
                "usb_pid": a.usb_pid,
                "is_wfb_compatible": a.is_wfb_compatible,
                "capabilities": a.capabilities,
            }
            for a in adapters
        ]
        click.echo(json.dumps(payload, indent=2, sort_keys=True))
        return

    if not adapters:
        click.echo(
            click.style(
                "No WiFi adapters detected. Plug in an RTL8812EU and retry.",
                fg="red",
            )
        )
        raise click.exceptions.Exit(code=1)

    for a in adapters:
        if a.is_wfb_compatible:
            verdict, colour = "WFB-capable", "green"
        elif a.supports_monitor:
            verdict, colour = "monitor only (not injection)", "yellow"
        else:
            verdict, colour = "not compatible", "red"
        chipset = a.chipset or a.driver or "unknown"
        line = (
            f"  {a.interface_name:<18}  {chipset:<22}  "
            f"mode={a.current_mode or '?':<8}  {verdict}"
        )
        click.echo(click.style(line, fg=colour))


# ---------------------------------------------------------------------
# Pair lifecycle: status, local bind, unpair, auto-pair toggle.
# ---------------------------------------------------------------------


@click.group("pair", help="Inspect and control WFB radio pairing.")
def pair_group() -> None:
    pass


radio_group.add_command(pair_group)


def _print_pair_status(data: dict[str, Any]) -> None:
    paired = bool(data.get("paired"))
    state = "Paired" if paired else "Not paired"
    click.echo(click.style(f"Pair state: {state}", bold=True))
    click.echo(f"  Role            {data.get('role') or '—'}")
    click.echo(f"  Peer device-id  {data.get('paired_with_device_id') or '—'}")
    click.echo(f"  Paired at       {data.get('paired_at') or '—'}")
    click.echo(f"  Fingerprint     {data.get('fingerprint') or '—'}")
    click.echo(
        f"  Auto-pair       "
        f"{'armed' if data.get('auto_pair_enabled') else 'disabled'}"
    )


@pair_group.command("status", help="Print pair state for this rig.")
@click.option("--json", "as_json", is_flag=True)
def pair_status(as_json: bool) -> None:
    _, body = _request("GET", "/api/wfb/pair")
    if as_json:
        click.echo(json.dumps(body, indent=2, sort_keys=True))
    else:
        _print_pair_status(body)


@pair_group.command(
    "local",
    help=(
        "Open a local-radio bind window. Drives the upstream wfb-ng bind "
        "protocol via the agent's REST. Returns the terminal session "
        "state when the protocol completes (≤60s)."
    ),
)
@click.option(
    "--role",
    type=click.Choice(["drone", "gs"]),
    default=None,
    help="Override the inferred profile role.",
)
@click.option(
    "--peer-device-id",
    "peer_device_id",
    default=None,
    help="Optional peer device-id to persist alongside pair state.",
)
def pair_local(role: str | None, peer_device_id: str | None) -> None:
    payload: dict[str, Any] = {}
    if role is not None:
        payload["role"] = role
    if peer_device_id is not None:
        payload["peer_device_id"] = peer_device_id
    code, body = _request(
        "POST",
        "/api/wfb/pair/local-bind",
        json=payload,
        raise_for_status=False,
        timeout=120.0,
    )
    if code == 409:
        click.echo(
            click.style("A bind session is already in progress.", fg="yellow")
        )
        raise click.exceptions.Exit(code=2)
    if code >= 400:
        click.echo(click.style(f"Bind failed: HTTP {code}: {body}", fg="red"))
        raise click.exceptions.Exit(code=1)

    state = body.get("state")
    if state == "paired":
        click.echo(click.style("Bind succeeded.", fg="green"))
        click.echo(f"  fingerprint   {body.get('fingerprint')}")
        click.echo(f"  finished_at   {body.get('finished_at')}")
    else:
        click.echo(
            click.style(
                f"Bind ended in state '{state}': {body.get('error') or '—'}",
                fg="red",
            )
        )
        raise click.exceptions.Exit(code=1)


@pair_group.command(
    "failover-status",
    help="Show local-bind to cloud-relay failover state.",
)
@click.option("--json", "as_json", is_flag=True, help="Emit JSON.")
def pair_failover_status(as_json: bool) -> None:
    _, body = _request("GET", "/api/wfb/pair/failover-status")
    if as_json:
        click.echo(json.dumps(body, indent=2, sort_keys=True))
        return
    state = body.get("failover_state", "local")
    if state == "local":
        click.echo("Local bind active.")
    elif state == "cloud_relay":
        click.echo(
            "Failover to cloud relay. "
            "Run `ados radio pair auto on` to retry local bind."
        )
    elif state == "failed":
        click.echo("Pairing failed.")
    else:
        click.echo(f"Unknown state: {state}")


@pair_group.command("unpair", help="Wipe pair keys and clear pair state.")
@click.option("--yes", is_flag=True, help="Skip the confirmation prompt.")
def pair_unpair(yes: bool) -> None:
    if not yes:
        click.confirm(
            "Wipe wfb-ng key files and clear pair state?",
            abort=True,
            default=False,
        )
    _, body = _request("POST", "/api/wfb/pair/unpair")
    click.echo("Unpaired.")
    if isinstance(body, dict) and body.get("role"):
        click.echo(f"  role  {body['role']}")


@pair_group.group("auto", help="Toggle auto-pair on first boot.")
def pair_auto_group() -> None:
    pass


@pair_auto_group.command("on", help="Re-arm auto-pair (only when unpaired).")
def pair_auto_on() -> None:
    code, body = _request(
        "PUT",
        "/api/wfb/pair/auto-pair",
        json={"enabled": True},
        raise_for_status=False,
    )
    if code >= 400:
        click.echo(click.style(f"Failed: HTTP {code}: {body}", fg="red"))
        raise click.exceptions.Exit(code=1)
    if body.get("rearm_blocked"):
        click.echo(
            click.style(
                "Cannot re-arm auto-pair while paired. "
                "Run `ados radio pair unpair` first.",
                fg="yellow",
            )
        )
        raise click.exceptions.Exit(code=2)
    click.echo("Auto-pair armed.")


@pair_auto_group.command("off", help="Disable auto-pair.")
def pair_auto_off() -> None:
    _, _body = _request(
        "PUT",
        "/api/wfb/pair/auto-pair",
        json={"enabled": False},
        raise_for_status=False,
    )
    click.echo("Auto-pair disabled.")


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
