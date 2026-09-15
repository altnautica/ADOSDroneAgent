"""The config migration pass. Runs off the startup path, under the lock.

This is the write half of what :mod:`ados.core.config._migrators` used to do
inline. It exists as a separate, explicitly-invoked pass because the read
path is not allowed to write:

* ``load_config()`` is on the startup path of every unit on the node, and
  they start concurrently. Eleven processes each doing a full-file
  read-modify-write of the file that carries the radio pairing key, the
  profile and the role is a lost-update race, and the loss is silent.
* The native writers serialise on ``/run/ados/config.yaml.lock``. Taking
  that lock from an import-time code path in eleven units would convert a
  write race into a boot-time lock convoy; taking it here, once, from a
  process nothing is waiting on, costs nothing.

The pass is a compare-and-swap, not a fire-and-forget: the read whose result
gets serialised happens **inside** the lock. A pass that decided its output
from a copy read before acquiring would write a mapping that predates
whatever the lock holder just committed, reverting it.

Invoked by ``ados config migrate`` (which the installer runs on every
install and upgrade). Idempotent by construction, so running it again is
free and running it on an already-current node writes nothing.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from ados.core.atomic import atomic_write_text
from ados.core.paths import CONFIG_MIGRATIONS_PATH, CONFIG_YAML

from ._lock import WRITE_LOCK_TIMEOUT_S
from ._migrators import ALL_MIGRATION_IDS, apply_migrations, apply_one_shots
from .writer import read_config_mapping, update_config


@dataclass
class MigrationResult:
    """What the pass did, in terms an operator surface can render."""

    path: Path
    locked: bool = False
    changed: bool = False
    applied: list[str] = field(default_factory=list)
    error: str | None = None

    @property
    def ok(self) -> bool:
        return self.error is None

    def summary(self) -> str:
        if self.error is not None:
            return f"{self.path}: {self.error}"
        if not self.changed:
            return f"{self.path}: already current"
        return f"{self.path}: applied {', '.join(self.applied)}"


def _ledger_mode() -> int:
    # The ledger records which one-shot cleanups a node has had. It carries no
    # secret, and an operator reading it is a supported diagnostic.
    return 0o644


def completed_one_shots(path: Path | None = None) -> set[str]:
    """Read the ledger of one-shot cleanups this node has already had.

    A missing or garbled ledger reads as empty. That direction is
    deliberate: re-running a one-shot removes a value some older build
    wrote, which is a no-op on a node that no longer has it. Reading a
    garbled ledger as *complete* would instead skip a cleanup a field node
    still needs.
    """
    ledger_path = path if path is not None else CONFIG_MIGRATIONS_PATH
    try:
        data = json.loads(ledger_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return set()
    applied = data.get("applied") if isinstance(data, dict) else None
    if not isinstance(applied, list):
        return set()
    return {item for item in applied if isinstance(item, str)}


def _record_one_shots(names: list[str], path: Path | None = None) -> None:
    """Add ``names`` to the ledger. Best-effort: a ledger we cannot write
    costs a repeated no-op cleanup, never a failed migration."""
    ledger_path = path if path is not None else CONFIG_MIGRATIONS_PATH
    merged = sorted(completed_one_shots(ledger_path) | set(names))
    try:
        atomic_write_text(
            ledger_path,
            json.dumps({"version": 1, "applied": merged}, indent=2) + "\n",
            mode=_ledger_mode(),
        )
    except OSError:
        pass


def migrate_config_file(
    path: str | Path | None = None,
    timeout_s: float = WRITE_LOCK_TIMEOUT_S,
    dry_run: bool = False,
    ledger_path: Path | None = None,
) -> MigrationResult:
    """Bring the on-disk config to the current shape.

    Returns a :class:`MigrationResult` rather than raising: this is invoked
    from the installer and the CLI, where a config that could not be
    migrated is a reportable condition, not a crash.

    ``dry_run`` reports what would change without writing. It still takes
    the lock, because the answer is only meaningful for a file nobody is
    mid-write on.

    ``ledger_path`` overrides where completed one-shot cleanups are
    recorded; the default is the node's own ledger.
    """
    config_path = Path(path) if path is not None else CONFIG_YAML
    result = MigrationResult(path=config_path)

    if not config_path.is_file():
        # Nothing to migrate and nothing to create: a node with no config
        # file runs on shipped defaults, which are by definition current.
        return result

    # `applied` is what a write would land; `changed` is what a write did
    # land. Keeping them separate is what makes `dry_run` honest and a failed
    # write honest in the same shape.
    one_shots: list[str] = []

    def _mutate(raw: dict[str, Any]) -> None:
        # Normalisers first, then the ledger-gated one-shots: a one-shot that
        # removes a key has to see the shape the normalisers produce, not the
        # legacy shape.
        done = completed_one_shots(ledger_path)
        one_shots.clear()
        one_shots.extend(apply_one_shots(raw, done))
        result.applied = apply_migrations(raw) + list(one_shots)

    write = update_config(
        _mutate, path=config_path, timeout_s=timeout_s, dry_run=dry_run
    )
    result.locked = write.locked
    if not write.ok:
        result.error = write.error
        return result

    result.changed = write.wrote

    # Only after the config write landed. Recording a one-shot whose write
    # failed would skip it on the retry that is supposed to fix the failure.
    if one_shots and result.changed:
        _record_one_shots(one_shots, ledger_path)

    return result


def pending_migrations(
    path: str | Path | None = None, ledger_path: Path | None = None
) -> list[str]:
    """Names of migrations a node still needs, without taking the lock.

    Read-only and side-effect free, for a status surface. Reports on a
    private copy, so nothing observable changes.
    """
    config_path = Path(path) if path is not None else CONFIG_YAML
    if not config_path.is_file():
        return []
    raw = read_config_mapping(config_path)
    done = completed_one_shots(ledger_path)
    return apply_migrations(raw) + apply_one_shots(raw, done)


def ledger_state(ledger_path: Path | None = None) -> dict[str, bool]:
    """Every known migration id mapped to whether it is a one-shot already
    recorded as done. Normalisers report ``False``: they are idempotent and
    are never recorded, so "done" is not a thing they can be."""
    done = completed_one_shots(ledger_path)
    return {name: name in done for name in ALL_MIGRATION_IDS}
