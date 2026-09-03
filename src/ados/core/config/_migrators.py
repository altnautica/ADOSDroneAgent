"""Legacy config normalisers — pure, in-memory, and side-effect free.

Every function here takes the raw mapping loaded from ``config.yaml``,
mutates it in place to the current shape, and reports whether it changed
anything. **None of them touches the disk**, and that is the point.

These used to write. Each one, on a hit, serialised the whole merged
mapping back over ``/etc/ados/config.yaml`` with a tmp-write and an
``os.replace``, from inside ``load_config()`` — which every one of the
node's units calls on its own startup path, concurrently, with no lock,
against the one file that carries the radio pairing key, the profile and
the role. The native writers take ``/run/ados/config.yaml.lock``; a lock
only one side takes protects nobody.

Splitting the normalisation from the persistence fixes both halves at once:

* the read path applies the current shape in memory, so an un-migrated
  node behaves correctly without writing anything;
* :mod:`ados.core.config.maintenance` persists the same result once, off
  the startup path, under the lock, re-reading the file inside it.

Each normaliser checks its **destination** before its **source**, so a
node that has already been migrated does no file I/O at all. That matters
because the legacy side files are deliberately preserved on disk for
rollback, so "already migrated" is the steady state, not a rare one.
"""

from __future__ import annotations

import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

from ados.core.paths import GS_UI_JSON

_LEGACY_GS_UI_PATH = GS_UI_JSON
_GS_UI_KEYS = ("oled", "buttons", "screens")


def _read_legacy_gs_ui() -> dict[str, Any] | None:
    """Parse the legacy ground-station-ui side file, or ``None``."""
    try:
        if not _LEGACY_GS_UI_PATH.is_file():
            return None
        data = json.loads(_LEGACY_GS_UI_PATH.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


def _section(raw: dict[str, Any], key: str) -> dict[str, Any]:
    """Return ``raw[key]`` as a dict, replacing a non-dict with a fresh one.

    Not created in ``raw`` unless the caller stores it back, so a
    normaliser that decides against a change leaves no empty section
    behind for the maintenance pass to persist.
    """
    value = raw.get(key)
    return value if isinstance(value, dict) else {}


def apply_share_uplink_from_legacy_json(raw: dict[str, Any]) -> bool:
    """Pull ``share_uplink`` out of the legacy ground-station-ui side file.

    A value already present under ``ground_station`` wins and is never
    overwritten, so this is a backfill, not a sync.
    """
    gs_section = _section(raw, "ground_station")
    if "share_uplink" in gs_section:
        return False

    legacy = _read_legacy_gs_ui()
    if legacy is None or "share_uplink" not in legacy:
        return False

    gs_section["share_uplink"] = bool(legacy.get("share_uplink", False))
    raw["ground_station"] = gs_section
    return True


def apply_gs_ui_from_legacy_json(raw: dict[str, Any]) -> bool:
    """Pull oled/buttons/screens out of the legacy side file.

    Per key: a key already present under ``ground_station.ui`` wins.
    """
    gs_section = _section(raw, "ground_station")
    ui_section = _section(gs_section, "ui")
    if all(key in ui_section for key in _GS_UI_KEYS):
        return False

    legacy = _read_legacy_gs_ui()
    if legacy is None:
        return False

    changed = False
    for key in _GS_UI_KEYS:
        if key in ui_section:
            continue
        legacy_value = legacy.get(key)
        if isinstance(legacy_value, dict):
            ui_section[key] = legacy_value
            changed = True

    if not changed:
        return False

    gs_section["ui"] = ui_section
    raw["ground_station"] = gs_section
    return True


def apply_api_from_scripting(raw: dict[str, Any]) -> bool:
    """Relocate the REST-API surface out of the legacy ``scripting`` block.

    The host/port for the agent's HTTP server and the optional Mission
    Control URL used to live under ``scripting.rest_api`` and
    ``scripting.mission_control_url``; they now live under ``api.rest`` and
    ``api.mission_control_url``. Per field: a value already present under
    ``api`` wins, so an operator who customised the REST port keeps it.
    """
    legacy = raw.get("scripting")
    if not isinstance(legacy, dict):
        return False

    legacy_rest = legacy.get("rest_api")
    legacy_mc_url = legacy.get("mission_control_url")
    if not isinstance(legacy_rest, dict) and legacy_mc_url is None:
        return False

    api_section = _section(raw, "api")
    changed = False

    if isinstance(legacy_rest, dict):
        rest_section = _section(api_section, "rest")
        for key in ("enabled", "host", "port"):
            if key in legacy_rest and key not in rest_section:
                rest_section[key] = legacy_rest[key]
                changed = True
        if rest_section:
            api_section["rest"] = rest_section

    if legacy_mc_url is not None and "mission_control_url" not in api_section:
        api_section["mission_control_url"] = legacy_mc_url
        changed = True

    if not changed:
        return False

    raw["api"] = api_section
    return True


def apply_ws_proxy_enforce_default(raw: dict[str, Any]) -> bool:
    """Drop a persisted ``mavlink.ws_proxy_enforce_auth: false``.

    The MAVLink WebSocket proxy used to log an unauthorized connection and
    serve it anyway, and that posture was the shipped default. Every node
    written while it was has ``false`` recorded in its own config file, so
    changing the default in code changes nothing on any of them: an
    explicit value wins over a default, which is the whole point of an
    explicit value. Proven on a bench node, where the proxy reported
    ``enforce_auth=false admitted=true`` while running a build whose
    default was ``true``.

    So the recorded value is removed rather than rewritten. Removing it
    lets the node follow the shipped posture now and in future, where
    writing ``true`` would freeze today's answer into the file and
    reproduce this same problem the next time the default moves.

    **This is a one-shot cleanup, not a normalisation, and the difference
    is load-bearing.** Removing a recorded ``false`` is only correct for a
    value some older build wrote. A ``false`` an operator sets *after* the
    cleanup has run is a deliberate opt-out — a node with a third-party
    client that cannot present a credential needs it — and re-applying
    this would silently override it on every read, forever. So it runs
    once per node, gated on the ledger in
    :mod:`ados.core.config.maintenance`, and never on the read path.
    """
    mav = raw.get("mavlink")
    if not isinstance(mav, dict) or mav.get("ws_proxy_enforce_auth") is not False:
        return False
    mav.pop("ws_proxy_enforce_auth", None)
    return True


Migration = Callable[[dict[str, Any]], bool]

# Idempotent shape translations. Each backfills a destination from a legacy
# source and lets an existing destination win, so applying it twice is a
# no-op and applying it on the read path is free of consequence. These run
# on both the read path and the maintenance pass, from this one list, so the
# two can never drift into applying different sets.
NORMALISERS: tuple[tuple[str, Migration], ...] = (
    ("share_uplink_from_legacy_json", apply_share_uplink_from_legacy_json),
    ("gs_ui_from_legacy_json", apply_gs_ui_from_legacy_json),
    ("api_from_scripting", apply_api_from_scripting),
)

# One-shot cleanups. These *remove* a recorded value, which is only correct
# the first time — after that, the same value means something different
# (an operator said it, not an old build). Run by the maintenance pass only,
# once per node, recorded in a persistent ledger. Never on the read path.
ONE_SHOTS: tuple[tuple[str, Migration], ...] = (
    ("ws_proxy_enforce_default", apply_ws_proxy_enforce_default),
)

ALL_MIGRATION_IDS: tuple[str, ...] = tuple(
    name for name, _ in (*NORMALISERS, *ONE_SHOTS)
)


def apply_migrations(raw: dict[str, Any]) -> list[str]:
    """Bring ``raw`` to the current shape in place; return what changed.

    Normalisers only — this is what the read path runs, and the read path
    must not apply a one-shot cleanup.
    """
    return [name for name, fn in NORMALISERS if fn(raw)]


def apply_one_shots(raw: dict[str, Any], completed: set[str]) -> list[str]:
    """Apply the one-shot cleanups not already recorded in ``completed``."""
    return [
        name for name, fn in ONE_SHOTS if name not in completed and fn(raw)
    ]


def legacy_gs_ui_path() -> Path:
    """The legacy side file, for operator-facing reporting."""
    return _LEGACY_GS_UI_PATH


def _deep_merge(base: dict[str, Any], override: dict[str, Any]) -> dict[str, Any]:
    """Merge override into base recursively."""
    merged = base.copy()
    for key, val in override.items():
        if key in merged and isinstance(merged[key], dict) and isinstance(val, dict):
            merged[key] = _deep_merge(merged[key], val)
        else:
            merged[key] = val
    return merged
