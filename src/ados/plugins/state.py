"""Persistent plugin install state.

Records what is installed, what is enabled, and what permissions are
granted on this device. The supervisor reconciles against this state
on boot and after any lifecycle transition. The GCS reads it via the
plugins REST API.

State file at ``/var/ados/state/plugin-state.json``. JSON shape:

    {
      "schema": 1,
      "installs": [
        {
          "plugin_id": "com.example.thermal",
          "version": "1.0.0",
          "source": "local_file",
          "source_uri": "/tmp/thermal-1.0.0.adosplug",
          "signer_id": "altnautica-2026-A",
          "manifest_hash": "<hex>",
          "status": "enabled",
          "installed_at": 1735000000,
          "enabled_at": 1735000123,
          "permissions": {
            "hardware.spi": {"granted": true, "granted_at": 1735000000},
            "vehicle.command": {"granted": false, "granted_at": null}
          }
        }
      ]
    }
"""

from __future__ import annotations

import contextlib
import fcntl
import json
import os
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Literal

from ados.core.logging import get_logger
from ados.core.paths import PLUGIN_STATE_PATH

log = get_logger("plugins.state")

PluginStatus = Literal[
    "installed",
    "enabled",
    "running",
    "disabled",
    "failed",
    "incompatible",
]

PluginSource = Literal["local_file", "git_url", "registry"]


@dataclass
class PermissionGrant:
    granted: bool
    granted_at: int | None
    revoked_at: int | None = None


@dataclass
class PluginInstall:
    plugin_id: str
    version: str
    source: PluginSource
    source_uri: str | None
    signer_id: str | None
    manifest_hash: str
    status: PluginStatus
    installed_at: int
    enabled_at: int | None = None
    failure_reason: str | None = None
    permissions: dict[str, PermissionGrant] = field(default_factory=dict)


@contextlib.contextmanager
def state_lock(path: Path | None = None):
    """Block-mode advisory file lock around a state read-modify-write.

    Wraps ``load_state`` + mutation + ``save_state`` so two concurrent
    install/remove flows on the same host serialize cleanly. The lock
    is held on a sidecar ``.lock`` file so ``state.json`` can be
    atomically replaced without invalidating the lock fd.
    """
    target = Path(path) if path is not None else PLUGIN_STATE_PATH
    lock_path = target.with_suffix(target.suffix + ".lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def filter_permissions_against_manifest(
    install: PluginInstall, declared: set[str]
) -> PluginInstall:
    """Drop any granted permission that the manifest no longer declares.

    Defends against a tampered state file granting permissions the
    plugin never asked for. Called on every load so the in-memory
    representation is always a subset of what the manifest authorizes.
    """
    bad_keys = [pid for pid in install.permissions.keys() if pid not in declared]
    if not bad_keys:
        return install
    log.warning(
        "plugin_state_permission_filtered",
        plugin_id=install.plugin_id,
        dropped=bad_keys,
    )
    install.permissions = {
        pid: grant
        for pid, grant in install.permissions.items()
        if pid in declared
    }
    return install


def _now_ms() -> int:
    return int(time.time() * 1000)


def load_state(path: Path | None = None) -> list[PluginInstall]:
    target = Path(path) if path is not None else PLUGIN_STATE_PATH
    if not target.exists():
        return []
    try:
        raw = json.loads(target.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        log.warning("plugin_state_read_failed", error=str(exc))
        return []
    if not isinstance(raw, dict):
        return []
    installs_raw = raw.get("installs") or []
    out: list[PluginInstall] = []
    for entry in installs_raw:
        try:
            perms = {
                pid: PermissionGrant(**(pgrant if isinstance(pgrant, dict) else {}))
                for pid, pgrant in (entry.get("permissions") or {}).items()
            }
            out.append(
                PluginInstall(
                    plugin_id=entry["plugin_id"],
                    version=entry["version"],
                    source=entry["source"],
                    source_uri=entry.get("source_uri"),
                    signer_id=entry.get("signer_id"),
                    manifest_hash=entry["manifest_hash"],
                    status=entry["status"],
                    installed_at=int(entry["installed_at"]),
                    enabled_at=entry.get("enabled_at"),
                    failure_reason=entry.get("failure_reason"),
                    permissions=perms,
                )
            )
        except (KeyError, TypeError) as exc:
            log.warning(
                "plugin_state_entry_skipped",
                error=str(exc),
                entry=entry,
            )
            continue
    return out


def save_state(installs: list[PluginInstall], path: Path | None = None) -> None:
    target = Path(path) if path is not None else PLUGIN_STATE_PATH
    target.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "schema": 1,
        "installs": [
            {
                **asdict(inst),
                "permissions": {
                    pid: asdict(pg) for pid, pg in inst.permissions.items()
                },
            }
            for inst in installs
        ],
    }
    tmp = target.with_suffix(target.suffix + ".tmp")
    tmp.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    tmp.replace(target)


def find_install(
    installs: list[PluginInstall], plugin_id: str
) -> PluginInstall | None:
    for inst in installs:
        if inst.plugin_id == plugin_id:
            return inst
    return None


def upsert_install(
    installs: list[PluginInstall], install: PluginInstall
) -> list[PluginInstall]:
    out = [i for i in installs if i.plugin_id != install.plugin_id]
    out.append(install)
    return out


def remove_install(
    installs: list[PluginInstall], plugin_id: str
) -> list[PluginInstall]:
    return [i for i in installs if i.plugin_id != plugin_id]


def grant_permission(
    install: PluginInstall, permission_id: str
) -> None:
    install.permissions[permission_id] = PermissionGrant(
        granted=True, granted_at=_now_ms()
    )


def revoke_permission(
    install: PluginInstall, permission_id: str
) -> None:
    grant = install.permissions.get(permission_id)
    if grant is None:
        return
    grant.granted = False
    grant.revoked_at = _now_ms()


def is_permission_granted(install: PluginInstall, permission_id: str) -> bool:
    grant = install.permissions.get(permission_id)
    return grant is not None and grant.granted
