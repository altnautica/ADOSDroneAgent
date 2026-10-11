"""``ados radio test`` and ``ados radio adapters`` subcommands."""

from __future__ import annotations

import json
import subprocess
import time

import click

from ados.cli._radio_base import _format_rssi, _request, radio_group


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

    # The restore value is the one the radio reported; with none reported the
    # sweep refuses to run rather than leaving a power level nobody set.
    original_tx = status_data.get("tx_power_dbm")
    if not isinstance(original_tx, int) or isinstance(original_tx, bool):
        click.echo(
            click.style(
                "The radio did not report its current TX power, so the sweep "
                "could not restore it afterwards. Not starting.",
                fg="red",
            )
        )
        raise click.exceptions.Exit(code=1)

    # A drone only transmits; the RSSI it reports is not a measurement of
    # its own downlink, so the sweep does not print one there.
    from ados.core.config import load_config
    from ados.core.profile import current_profile_and_role

    rssi_measured = current_profile_and_role(load_config())[0] != "drone"

    click.echo(
        click.style(
            f"Starting TX-power sweep. Will restore tx_power_dbm={original_tx} at the end.",
            bold=True,
        )
    )

    try:
        for dbm in (1, 5, 10):
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

            if rssi_measured:
                _, post_status = _request("GET", "/api/wfb")
                rssi = post_status.get("rssi_dbm")
                if rssi == -100.0:
                    rssi = None
                rssi_text = _format_rssi(rssi)
            else:
                rssi_text = "not measured on a drone (read it on the ground station)"
            click.echo(f"  effective={body.get('effective_dbm')} dBm   RSSI={rssi_text}")

            kernel_tail = _read_kernel_log_tail()
            flagged = [line for line in kernel_tail if "undervoltage" in line.lower()]
            if flagged:
                click.echo(click.style("  Kernel undervoltage warnings:", fg="yellow"))
                for line in flagged:
                    click.echo(f"    {line}")
    finally:
        # Runs on an HTTP error and on Ctrl-C too: a bench sweep must never
        # leave a test power level persisted on the node.
        click.echo("")
        click.echo(click.style(f"Restoring TX power to {original_tx} dBm", fg="cyan"))
        code, body = _request(
            "PUT",
            "/api/wfb/tx-power",
            json={"tx_power_dbm": original_tx},
            raise_for_status=False,
        )
        if code >= 400:
            click.echo(
                click.style(
                    f"  Restore refused: HTTP {code}: {body}. Set it back with "
                    f"`ados radio set-tx-power {original_tx}`.",
                    fg="red",
                )
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
