"""``ados config`` CLI subcommand tree.

The config migration pass, reachable over SSH. It used to have no
entrypoint at all, because it ran implicitly inside ``load_config()`` — on
the startup path of every unit on the node, concurrently, with no lock,
rewriting the file that carries the radio pairing key, the profile and the
role. Moving it off that path means it needs somewhere to be invoked from:
the installer calls this on every install and upgrade, and an operator can
run it directly when a node reports pending migrations.

Two commands:

* ``ados config migrate`` — take the config lock, bring the file to the
  current shape, and report what changed. ``--check`` reports without
  writing.
* ``ados config status`` — what the read path is normalising in memory and
  which one-shot cleanups this node has already had, with no lock taken
  and nothing written.
"""

from __future__ import annotations

import json as _json

import click

from ados.core.config.maintenance import (
    ledger_state,
    migrate_config_file,
    pending_migrations,
)
from ados.core.paths import CONFIG_MIGRATIONS_PATH, CONFIG_YAML


@click.group("config", help="Inspect and migrate the on-disk agent config.")
def config_group() -> None:
    pass


@config_group.command(
    "migrate",
    help="Bring /etc/ados/config.yaml to the current shape, under the lock.",
)
@click.option(
    "--check",
    is_flag=True,
    help="Report what would change without writing.",
)
@click.option("--json", "as_json", is_flag=True, help="Machine-readable output.")
def migrate(check: bool, as_json: bool) -> None:
    result = migrate_config_file(dry_run=check)

    if as_json:
        click.echo(
            _json.dumps(
                {
                    "path": str(result.path),
                    "locked": result.locked,
                    "changed": result.changed,
                    "applied": result.applied,
                    "error": result.error,
                    "dry_run": check,
                },
                indent=2,
            )
        )
    elif result.error is not None:
        click.echo(f"error: {result.error}", err=True)
    elif not result.applied:
        click.echo(f"{result.path}: already current")
    elif check:
        click.echo(f"{result.path}: would apply {', '.join(result.applied)}")
    else:
        click.echo(f"{result.path}: applied {', '.join(result.applied)}")

    # A lock we could not take is not a failure of the config; it is a
    # concurrent writer, and the right answer is to run again. Exit non-zero
    # so an installer step or a shell loop can tell.
    if result.error is not None:
        raise SystemExit(1)


@config_group.command(
    "status", help="Show pending config migrations without writing anything."
)
@click.option("--json", "as_json", is_flag=True, help="Machine-readable output.")
def status(as_json: bool) -> None:
    pending = pending_migrations()
    ledger = ledger_state()

    if as_json:
        click.echo(
            _json.dumps(
                {
                    "path": str(CONFIG_YAML),
                    "ledger": str(CONFIG_MIGRATIONS_PATH),
                    "pending": pending,
                    "one_shots_done": sorted(k for k, v in ledger.items() if v),
                },
                indent=2,
            )
        )
        return

    click.echo(f"config: {CONFIG_YAML}")
    if pending:
        click.echo(f"pending: {', '.join(pending)}")
        click.echo("run `sudo ados config migrate` to persist")
    else:
        click.echo("pending: none")
    done = sorted(k for k, v in ledger.items() if v)
    click.echo(f"one-shot cleanups recorded: {', '.join(done) if done else 'none'}")
