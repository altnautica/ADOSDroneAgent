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
import subprocess
import time
from typing import Any

import click
import httpx

from ados.core.paths import PAIRING_JSON

API_BASE = "http://localhost:8080"


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


def _print_status_table(data: dict[str, Any]) -> None:
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

    click.echo(click.style(f"WFB-ng radio  state={state}", bold=True))
    click.echo("")

    rows: list[tuple[str, str, str | None]] = [
        ("Interface", str(iface), None),
        ("Driver / chipset", f"{driver} / {chipset}", None),
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
    if as_json:
        click.echo(json.dumps(data, indent=2, sort_keys=True))
    else:
        _print_status_table(data)
    if data.get("state") == "absent" or data.get("state") == "disabled":
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
