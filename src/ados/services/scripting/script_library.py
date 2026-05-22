"""Persistent script library — disk-backed save / list / delete.

The agent runs in multiple processes (``ados-api``, ``ados-cloud``,
``ados-supervisor``, ``ados-mavlink``) and the script runner only
lives in the supervisor. The REST routes are served by ``ados-api``.
Without a cross-process bridge the routes saw ``runner is None`` and
returned 503 even though the underlying operation (write a JSON file
to ``/var/ados/scripts/``) doesn't need the runner at all.

This module owns the saved-script library: pure disk operations that
any process can call without touching the runner. ``ScriptRunner``
delegates here so existing callers keep working; the routes call
straight in.

The execution path (``start_saved_script``) still lives on
``ScriptRunner`` because subprocess management is per-process state.
"""

from __future__ import annotations

import json
import os
import re
import uuid
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import SCRIPTS_DIR

log = get_logger("scripting.script_library")

# Reverse-DNS-style script id. Generated server-side as a 12-char hex
# so we never trust client-supplied ids on the filesystem.
SAVED_ID_RE = re.compile(r"^[a-f0-9]{12}$")

# Hard caps on the persistent library. A paired operator who buggies
# out (or a malicious caller who slipped through the auth gate) cannot
# fill /var/ados/scripts/ until the partition wedges agent operation.
MAX_SAVED_SCRIPTS = 256
MAX_SCRIPT_CONTENT_BYTES = 256 * 1024  # 256 KiB


@dataclass
class SavedScript:
    """Persisted script source as the dashboard / GCS see it. Mirrors
    Mission Control's ``ScriptInfo`` TypeScript interface verbatim so
    the API response is consumable without an adapter layer."""

    id: str
    name: str
    content: str
    suite: str | None = None
    lastModified: str = ""


def _saved_path(script_id: str) -> Path:
    return SCRIPTS_DIR / f"{script_id}.json"


def _read_saved(script_id: str) -> SavedScript | None:
    if not SAVED_ID_RE.fullmatch(script_id):
        return None
    path = _saved_path(script_id)
    if not path.is_file():
        return None
    try:
        raw = path.read_text(encoding="utf-8")
        data = json.loads(raw)
        return SavedScript(
            id=str(data.get("id", script_id)),
            name=str(data.get("name", "")),
            content=str(data.get("content", "")),
            suite=data.get("suite"),
            lastModified=str(data.get("lastModified", "")),
        )
    except (OSError, json.JSONDecodeError) as exc:
        log.warning("saved_script_unreadable", script_id=script_id, error=str(exc))
        return None


def list_saved_scripts() -> list[SavedScript]:
    """Return every persisted script as a SavedScript record."""
    out: list[SavedScript] = []
    try:
        SCRIPTS_DIR.mkdir(parents=True, exist_ok=True)
        for path in sorted(SCRIPTS_DIR.glob("*.json")):
            script_id = path.stem
            saved = _read_saved(script_id)
            if saved is not None:
                out.append(saved)
    except OSError as exc:
        log.warning("scripts_dir_unreadable", error=str(exc))
    return out


def get_saved_script(script_id: str) -> SavedScript | None:
    return _read_saved(script_id)


def save_script(
    name: str,
    content: str,
    suite: str | None = None,
) -> SavedScript:
    """Create or update a saved script. The id is server-assigned on
    first save; subsequent saves with the same name replace the
    existing record in place to keep the (name -> id) mapping stable
    for the dashboard."""
    name = name.strip()
    if not name:
        raise RuntimeError("script name required")
    content_bytes = content.encode("utf-8")
    if len(content_bytes) > MAX_SCRIPT_CONTENT_BYTES:
        raise RuntimeError(
            f"script content exceeds {MAX_SCRIPT_CONTENT_BYTES} bytes",
        )
    SCRIPTS_DIR.mkdir(parents=True, exist_ok=True)
    # Reuse the existing id when the operator saves under the same
    # name; treat name as the natural key for the dashboard while
    # keeping the on-disk file keyed by stable id.
    existing = next(
        (s for s in list_saved_scripts() if s.name == name),
        None,
    )
    if existing is None and len(list_saved_scripts()) >= MAX_SAVED_SCRIPTS:
        raise RuntimeError(
            f"script library full (max {MAX_SAVED_SCRIPTS} scripts)",
        )
    script_id = existing.id if existing else uuid.uuid4().hex[:12]
    record = SavedScript(
        id=script_id,
        name=name,
        content=content,
        suite=suite,
        lastModified=datetime.now(timezone.utc).isoformat(),
    )
    path = _saved_path(script_id)
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(asdict(record), indent=2), encoding="utf-8")
    os.replace(tmp, path)
    try:
        os.chmod(path, 0o600)
    except OSError:
        pass
    log.info("script_saved", script_id=script_id, name=name)
    return record


def delete_script(script_id: str) -> bool:
    """Remove a saved script. Returns False when the id is malformed
    or the record was already gone — never raises."""
    if not SAVED_ID_RE.fullmatch(script_id):
        return False
    path = _saved_path(script_id)
    try:
        path.unlink()
    except FileNotFoundError:
        return False
    except OSError as exc:
        log.warning("script_delete_failed", script_id=script_id, error=str(exc))
        return False
    log.info("script_deleted", script_id=script_id)
    return True
