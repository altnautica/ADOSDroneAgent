"""The one writer for the node's config document.

Every process that persists ``/etc/ados/config.yaml`` goes through
:func:`update_config`. There is deliberately exactly one lock window, one
atomic write and one post-write marker sync in the tree, because the file is
co-owned: the Python models describe part of it, the Rust services
(``ados-radio``, ``ados-supervisor``, ``ados-plugin-host``, the installer's
watchdog step) read keys the Python models have never heard of, and the
installer line-edits a third set.

Three rules follow from that co-ownership, and all three are enforced here
rather than left to each callsite:

**A write is a merge, never a replace.** The on-disk mapping is read *inside*
the write lock and the caller's change is applied on top of it. A key no
participant in this process knows about round-trips verbatim. The defect this
replaces was ``_save_config_dict(config.model_dump())``: a single-field PUT
serialised the whole Pydantic model over the file, which deleted every
Rust-owned key (``mavlink.injector_arbitration``, ``network.watchdog.enabled``,
``agent.headless``, ``video.wfb.reg_gate_strict``, …) and, because a dump
materialises defaults, froze that release's defaults into the node's file so no
later default change could ever reach it.

**Only what the caller set is written.** Callers name the leaves they change as
dotted paths (:func:`set_config_values`) or a nested change mapping
(:func:`merge_into_config`). Nothing is diffed against an in-memory model: a
model loaded earlier in a long-lived process is a stale copy of a file other
writers (the native routes, the CLI) change underneath it, and writing its
values back would revert theirs. An untouched default stays absent from the
file and keeps tracking the shipped value.

**Legacy shapes are not normalised here.** The in-memory normalisers
(``_migrators.NORMALISERS``) run only to validate a candidate document.
Persisting them is :mod:`ados.core.config.maintenance`'s job, which owns the
one-shot ledger. A config write must not silently land a migration whose
ledger row it does not write.
"""

from __future__ import annotations

import copy
import os
import stat
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml
from pydantic import BaseModel, ValidationError

from ados.core.atomic import atomic_write_text
from ados.core.paths import CONFIG_LOCK, CONFIG_YAML

from ._defaults import packaged_defaults
from ._lock import WRITE_LOCK_TIMEOUT_S, exclusive_config_lock
from ._migrators import _deep_merge, apply_migrations
from ._yaml import dump_mapping, read_mapping

# The document carries secrets (mqtt_password, api_key, hmac_secret, the AP
# passphrase, pair fingerprints). A file this writer creates, or one whose mode
# it could not read, gets the restrictive mode rather than the umask default.
SECRET_MODE = 0o600


@dataclass(frozen=True)
class ConfigWriteResult:
    """Outcome of one config write, in the shape a surface can report.

    Truthy when the write landed, while a caller that owes the operator a
    reason reads :attr:`error`. ``ok`` with an empty :attr:`changed` means the
    document already said what the caller wanted — nothing was written and
    nothing failed.
    """

    ok: bool
    error: str | None = None
    changed: tuple[str, ...] = field(default_factory=tuple)
    #: Whether the exclusive flock was actually held. A write is never
    #: attempted without it; a caller that must report *why* nothing landed
    #: distinguishes contention from an unwritable or unparseable file.
    locked: bool = False
    #: Whether bytes reached the file. False for a no-op write (the document
    #: already said this) and for a dry run.
    wrote: bool = False

    def __bool__(self) -> bool:
        return self.ok


def _lock_path_for(config_path: Path) -> Path:
    """The flock file guarding ``config_path``.

    The canonical document shares ``/run/ados/config.yaml.lock`` with the
    native writers — a lock only one participant takes is not a lock. A
    non-canonical path (a test fixture, an installer staging copy) gets a
    sibling lock so it neither contends on nor is serialised by the node's.
    """
    if config_path == CONFIG_YAML:
        return CONFIG_LOCK
    return config_path.with_name(config_path.name + ".lock")


def _mode_for(config_path: Path) -> int:
    try:
        return stat.S_IMODE(os.stat(config_path).st_mode)
    except OSError:
        return SECRET_MODE


def read_config_mapping(path: str | Path | None = None) -> dict[str, Any]:
    """The raw on-disk mapping, tolerating absence and garbage.

    Returns ``{}`` for a missing or unparseable file. Callers that need the
    document to feed a write must not use this — :func:`update_config` takes
    its own read inside the lock, which is the only read a write may rely on.
    """
    config_path = Path(path) if path is not None else CONFIG_YAML
    if not config_path.is_file():
        return {}
    try:
        return read_mapping(config_path)
    except (OSError, yaml.YAMLError):
        return {}


def _sync_markers(config_path: Path, previous: dict[str, Any], merged: dict[str, Any]) -> None:
    """Keep the sidecar enable-markers and units true to the document just written.

    Three lanes mirror a config flag onto a ``/etc/ados`` marker plus a
    ``systemctl try-reload-or-restart``: CRSF, the config-over-radio tunnel and
    the setup AP. A fourth starts or stops the remote-access tunnel unit. Each
    no-ops unless its own slice changed. Best-effort by contract — a marker or
    systemd hiccup never fails a write that has already landed on disk.

    Only runs for the node's canonical document. A staging or fixture path does
    not describe this node, so syncing this node's markers from it would be
    wrong, not merely untidy.
    """
    if config_path != CONFIG_YAML:
        return

    from ados.core.logging import get_logger

    log = get_logger("config.writer")
    for module_name, label in (
        ("ados.core.crsf_marker", "crsf"),
        ("ados.core.tunnel_marker", "tunnel"),
        ("ados.core.hotspot_marker", "hotspot"),
        ("ados.core.remote_access_sync", "remote_access"),
    ):
        try:
            module = __import__(module_name, fromlist=["sync_after_config_write"])
            module.sync_after_config_write(previous or None, merged)
        except Exception as exc:  # noqa: BLE001 — the write already landed
            log.warning(f"{label}_config_sync_failed", error=str(exc))


def update_config(
    mutate: Callable[[dict[str, Any]], Any],
    *,
    path: str | Path | None = None,
    timeout_s: float = WRITE_LOCK_TIMEOUT_S,
    changed: tuple[str, ...] = (),
    dry_run: bool = False,
) -> ConfigWriteResult:
    """Apply ``mutate`` to the on-disk mapping, atomically, under the lock.

    ``mutate`` receives the parsed document and edits it in place; it may add,
    change *or remove* keys, which is why the primitive is a mutator rather than
    a merge — the pair-state and factory-reset paths delete keys, and a merge
    cannot express a deletion.

    The read, the mutation and the write all happen inside one exclusive flock
    window, so two concurrent writers serialise instead of losing an update. A
    mutation that changes nothing writes nothing.

    ``dry_run`` still takes the lock and still runs ``mutate`` — the answer is
    only meaningful for a document nobody is mid-write on — but nothing reaches
    disk and no marker is synced.
    """
    config_path = Path(path) if path is not None else CONFIG_YAML

    with exclusive_config_lock(timeout_s, path=_lock_path_for(config_path)) as acquired:
        if not acquired:
            # Writing without the lock is the lost-update hazard the lock
            # exists to prevent: another writer is mid read-modify-write and
            # this write would serialise a document that predates its commit.
            return ConfigWriteResult(
                ok=False,
                error=(
                    f"config lock {_lock_path_for(config_path)} unavailable after "
                    f"{timeout_s:g}s; another writer holds it"
                ),
            )

        if config_path.is_file():
            try:
                document = read_mapping(config_path)
            except (OSError, yaml.YAMLError) as exc:
                # Never write over a document we could not parse: that write is
                # a truncation of whatever the operator actually has.
                return ConfigWriteResult(
                    ok=False,
                    locked=True,
                    error=f"{config_path} is unreadable or unparseable: {exc}",
                )
        else:
            document = {}

        previous = copy.deepcopy(document)
        try:
            mutate(document)
        except Exception as exc:  # noqa: BLE001 — a caller bug is still a failed write
            return ConfigWriteResult(
                ok=False, locked=True, error=f"config update failed: {exc}"
            )

        if document == previous:
            return ConfigWriteResult(ok=True, locked=True)

        try:
            body = dump_mapping(document)
        except yaml.YAMLError as exc:
            return ConfigWriteResult(
                ok=False, locked=True, error=f"config is not serialisable: {exc}"
            )

        if dry_run:
            return ConfigWriteResult(ok=True, locked=True, changed=changed)

        try:
            atomic_write_text(config_path, body, mode=_mode_for(config_path))
        except PermissionError as exc:
            return ConfigWriteResult(
                ok=False,
                locked=True,
                error=(
                    f"{config_path} is not writable by this process "
                    f"(config writes require root): {exc}"
                ),
            )
        except OSError as exc:
            return ConfigWriteResult(
                ok=False, locked=True, error=f"config write failed: {exc}"
            )

        _sync_markers(config_path, previous, document)
        return ConfigWriteResult(ok=True, locked=True, wrote=True, changed=changed)


def merge_into_config(
    changes: Mapping[str, Any],
    *,
    path: str | Path | None = None,
    timeout_s: float = WRITE_LOCK_TIMEOUT_S,
) -> ConfigWriteResult:
    """Deep-merge a nested ``changes`` mapping into the on-disk document.

    Mapping values recurse; every other value (a scalar, a list, a free-form
    ``dict[str, …]`` config field) replaces wholesale. Keys absent from
    ``changes`` are untouched.
    """
    flat = tuple(_dotted_paths(changes))
    if not flat:
        return ConfigWriteResult(ok=True)
    override = dict(changes)

    def _mutate(document: dict[str, Any]) -> None:
        merged = _deep_merge(document, override)
        document.clear()
        document.update(merged)

    return update_config(_mutate, path=path, timeout_s=timeout_s, changed=flat)


def set_config_values(
    values: Mapping[str, Any],
    *,
    path: str | Path | None = None,
    timeout_s: float = WRITE_LOCK_TIMEOUT_S,
    model: type[BaseModel] | None = None,
) -> ConfigWriteResult:
    """Set dotted-path leaves (``{"video.wfb.fec_k": 8}``) in the document.

    Each value is assigned to its leaf, not merged into it, and missing parent
    mappings are materialised. Siblings of the leaf are untouched.

    With ``model``, the resulting document must still validate as that model
    (built exactly as the loader builds it) or nothing is written. An
    out-of-range value would otherwise land on disk and make a daemon that
    parses the section strictly fall back to defaults for all of it.
    """
    leaves = [(tuple(dotted.split(".")), value) for dotted, value in values.items()]
    if not leaves:
        return ConfigWriteResult(ok=True)

    def _mutate(document: dict[str, Any]) -> None:
        for leaf, value in leaves:
            _assign(document, leaf, value)
        if model is not None:
            try:
                baseline_model(model, document)
            except ValidationError as exc:
                raise ValueError(f"the resulting config does not validate: {exc}") from exc

    return update_config(
        _mutate,
        path=path,
        timeout_s=timeout_s,
        changed=tuple(".".join(leaf) for leaf, _ in leaves),
    )


def _dotted_paths(changes: Mapping[str, Any], prefix: str = "") -> list[str]:
    """Leaf paths in a nested change mapping, for the write's ``changed`` report."""
    out: list[str] = []
    for key, value in changes.items():
        dotted = f"{prefix}{key}"
        if isinstance(value, Mapping) and value:
            out.extend(_dotted_paths(value, f"{dotted}."))
        else:
            out.append(dotted)
    return out


def _assign(document: dict[str, Any], path: tuple[str, ...], value: Any) -> None:
    """Set one leaf in ``document``, materialising missing parent mappings."""
    cursor = document
    for part in path[:-1]:
        nxt = cursor.get(part)
        if not isinstance(nxt, dict):
            nxt = {}
            cursor[part] = nxt
        cursor = nxt
    cursor[path[-1]] = value


def baseline_model(model_cls: type[BaseModel], document: Mapping[str, Any]) -> BaseModel:
    """What ``document`` means to ``model_cls``, built exactly as the loader does.

    ``load_config`` normalises the raw mapping in memory and merges it over the
    packaged defaults before validating. Validation of a candidate document has
    to do the same, or a legacy shape the loader accepts would be refused here.
    """
    raw = copy.deepcopy(dict(document))
    apply_migrations(raw)
    return model_cls(**_deep_merge(packaged_defaults(), raw))


__all__ = [
    "SECRET_MODE",
    "ConfigWriteResult",
    "baseline_model",
    "merge_into_config",
    "read_config_mapping",
    "set_config_values",
    "update_config",
]
