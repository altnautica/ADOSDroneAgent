"""``ados hardware`` CLI subcommand tree.

Tiny operator-facing surface for the persisted hardware-check
snapshot. The two commands today:

* ``ados hardware show`` — print the current snapshot. Useful from
  a serial console or SSH session where the GCS isn't open.
* ``ados hardware bust-cache`` — delete the persisted snapshot so
  the next API call re-probes. Wired to ``udev`` add/remove rules
  so a hot-plugged camera or FC pen cable is reflected on the
  dashboard within ~one polling tick instead of waiting for the
  30 s TTL to expire.

The CLI never blocks on a probe itself; it only manipulates the
on-disk snapshot. The agent does the actual probing on the next
read.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone

import click

from ados.setup import hardware_state


@click.group("hardware", help="Inspect and bust the hardware-check cache.")
def hardware_group() -> None:
    pass


@hardware_group.command("show", help="Print the persisted hardware snapshot.")
@click.option("--json", "as_json", is_flag=True, help="Machine-readable output.")
def show(as_json: bool) -> None:
    snap = hardware_state.read()
    if snap is None:
        if as_json:
            click.echo(json.dumps({"ok": False, "kind": "no_snapshot"}))
        else:
            click.echo("No hardware snapshot persisted yet.")
            click.echo(f"Snapshot path: {hardware_state.HARDWARE_STATE_PATH}")
        raise click.exceptions.Exit(code=1)

    if as_json:
        click.echo(json.dumps(snap.model_dump(mode="json"), sort_keys=True))
        return

    age_str = "?"
    try:
        ts = datetime.fromisoformat(snap.last_run)
        if ts.tzinfo is None:
            ts = ts.replace(tzinfo=timezone.utc)
        age = int((datetime.now(tz=ts.tzinfo) - ts).total_seconds())
        age_str = f"{age}s ago"
    except ValueError:
        pass

    click.echo(
        f"Profile: {snap.profile}"
        + (f" / {snap.ground_role}" if snap.ground_role else "")
        + f"  (last_run {age_str})"
    )
    icon = {
        "ok": "OK ",
        "warning": "WARN",
        "missing": "MISS",
        "checking": "... ",
        "unknown": "?  ",
    }
    required = [i for i in snap.items if i.required]
    optional = [i for i in snap.items if not i.required]
    if required:
        click.echo("Required:")
        for item in required:
            tag = icon.get(item.state, "?  ")
            click.echo(f"  [{tag}] {item.label:30s} {item.detail[:60]}")
    if optional:
        click.echo("Optional:")
        for item in optional:
            tag = icon.get(item.state, "?  ")
            click.echo(f"  [{tag}] {item.label:30s} {item.detail[:60]}")


@hardware_group.command(
    "bust-cache",
    help=(
        "Invalidate the persisted hardware snapshot so the next API "
        "call re-probes. Used by udev rules on USB add/remove."
    ),
)
@click.option("--quiet", is_flag=True, help="Suppress success message.")
def bust_cache(quiet: bool) -> None:
    hardware_state.clear()
    if not quiet:
        click.echo("Hardware cache cleared. Next API call will re-probe.")
