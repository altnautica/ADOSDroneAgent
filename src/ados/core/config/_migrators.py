"""Legacy config-file migrators (idempotent, one-shot per process)."""

from __future__ import annotations

import os as _os
from pathlib import Path
from typing import Any

import yaml

from ados.core.paths import GS_UI_JSON

# One-shot per-process guard. Keeps the INFO log from spamming even
# though the migrator is cheap and idempotent after the first run.
_SHARE_UPLINK_MIGRATED: bool = False
_GS_UI_MIGRATED: bool = False
_API_FROM_SCRIPTING_MIGRATED: bool = False

_LEGACY_GS_UI_PATH = GS_UI_JSON
_GS_UI_KEYS = ("oled", "buttons", "screens")


def _migrate_share_uplink_from_legacy_json(
    raw: dict[str, Any],
    yaml_path: Path | None,
) -> dict[str, Any]:
    """Pull `share_uplink` out of the legacy ground-station-ui.json side-file.

    Runs at most once per process (guarded by `_SHARE_UPLINK_MIGRATED`)
    and is a no-op if:
    - the legacy file does not exist, OR
    - the legacy file has no `share_uplink` key, OR
    - `raw['ground_station']['share_uplink']` is already set (Pydantic
      value wins).

    On a live migration the resolved value is written into `raw`
    in-memory AND flushed back to the on-disk YAML so later reads see
    the Pydantic field without needing the legacy file. The legacy
    JSON is preserved on disk for rollback and audit.
    """
    global _SHARE_UPLINK_MIGRATED
    if _SHARE_UPLINK_MIGRATED:
        return raw

    try:
        if not _LEGACY_GS_UI_PATH.is_file():
            _SHARE_UPLINK_MIGRATED = True
            return raw

        import json

        try:
            legacy_data = json.loads(
                _LEGACY_GS_UI_PATH.read_text(encoding="utf-8")
            )
        except (OSError, ValueError):
            _SHARE_UPLINK_MIGRATED = True
            return raw

        if not isinstance(legacy_data, dict):
            _SHARE_UPLINK_MIGRATED = True
            return raw

        if "share_uplink" not in legacy_data:
            _SHARE_UPLINK_MIGRATED = True
            return raw

        gs_section = raw.get("ground_station")
        if not isinstance(gs_section, dict):
            gs_section = {}
        if "share_uplink" in gs_section:
            # Pydantic config already has a value. Do not overwrite.
            _SHARE_UPLINK_MIGRATED = True
            return raw

        legacy_value = bool(legacy_data.get("share_uplink", False))
        gs_section["share_uplink"] = legacy_value
        raw["ground_station"] = gs_section

        # Flush to disk so subsequent loads do not need the legacy file.
        # Best-effort: on failure we still return the in-memory merge.
        if yaml_path is not None:
            try:
                to_write: dict[str, Any] = {}
                if yaml_path.is_file():
                    with open(yaml_path, encoding="utf-8") as fh:
                        loaded = yaml.safe_load(fh)
                    if isinstance(loaded, dict):
                        to_write = loaded
                disk_gs = to_write.get("ground_station")
                if not isinstance(disk_gs, dict):
                    disk_gs = {}
                disk_gs["share_uplink"] = legacy_value
                to_write["ground_station"] = disk_gs

                body = yaml.safe_dump(
                    to_write,
                    sort_keys=False,
                    default_flow_style=False,
                )
                yaml_path.parent.mkdir(parents=True, exist_ok=True)
                tmp_path = yaml_path.with_suffix(yaml_path.suffix + ".tmp")
                tmp_path.write_text(body, encoding="utf-8")
                _os.replace(str(tmp_path), str(yaml_path))
            except (OSError, yaml.YAMLError):
                # Non-fatal. In-memory value still applies for this run.
                pass

        # Log once. Use plain logging to avoid a circular import on
        # `ados.core.logging`, which itself may call `load_config()`.
        import logging as _logging

        _logging.getLogger("ados.core.config").info(
            f"migrated share_uplink from {GS_UI_JSON} (legacy file preserved)"
        )
    finally:
        _SHARE_UPLINK_MIGRATED = True

    return raw


def _migrate_gs_ui_from_legacy_json(
    raw: dict[str, Any],
    yaml_path: Path | None,
) -> dict[str, Any]:
    """Pull oled/buttons/screens out of the legacy ground-station-ui.json side-file.

    Same shape as `_migrate_share_uplink_from_legacy_json`. Per-key check:
    if `raw['ground_station']['ui'][key]` is already set, do not overwrite.
    Legacy file is preserved on disk for rollback.
    """
    global _GS_UI_MIGRATED
    if _GS_UI_MIGRATED:
        return raw

    try:
        if not _LEGACY_GS_UI_PATH.is_file():
            _GS_UI_MIGRATED = True
            return raw

        import json

        try:
            legacy_data = json.loads(
                _LEGACY_GS_UI_PATH.read_text(encoding="utf-8")
            )
        except (OSError, ValueError):
            _GS_UI_MIGRATED = True
            return raw

        if not isinstance(legacy_data, dict):
            _GS_UI_MIGRATED = True
            return raw

        gs_section = raw.get("ground_station")
        if not isinstance(gs_section, dict):
            gs_section = {}
        ui_section = gs_section.get("ui")
        if not isinstance(ui_section, dict):
            ui_section = {}

        merged_any = False
        for key in _GS_UI_KEYS:
            if key in legacy_data and isinstance(legacy_data[key], dict):
                if key not in ui_section:
                    ui_section[key] = legacy_data[key]
                    merged_any = True

        if not merged_any:
            _GS_UI_MIGRATED = True
            return raw

        gs_section["ui"] = ui_section
        raw["ground_station"] = gs_section

        if yaml_path is not None:
            try:
                to_write: dict[str, Any] = {}
                if yaml_path.is_file():
                    with open(yaml_path, encoding="utf-8") as fh:
                        loaded = yaml.safe_load(fh)
                    if isinstance(loaded, dict):
                        to_write = loaded
                disk_gs = to_write.get("ground_station")
                if not isinstance(disk_gs, dict):
                    disk_gs = {}
                disk_ui = disk_gs.get("ui")
                if not isinstance(disk_ui, dict):
                    disk_ui = {}
                for key in _GS_UI_KEYS:
                    if key in ui_section and key not in disk_ui:
                        disk_ui[key] = ui_section[key]
                disk_gs["ui"] = disk_ui
                to_write["ground_station"] = disk_gs

                body = yaml.safe_dump(
                    to_write,
                    sort_keys=False,
                    default_flow_style=False,
                )
                yaml_path.parent.mkdir(parents=True, exist_ok=True)
                tmp_path = yaml_path.with_suffix(yaml_path.suffix + ".tmp")
                tmp_path.write_text(body, encoding="utf-8")
                _os.replace(str(tmp_path), str(yaml_path))
            except (OSError, yaml.YAMLError):
                pass

        import logging as _logging

        _logging.getLogger("ados.core.config").info(
            "migrated ground_station.ui (oled/buttons/screens) from "
            f"{GS_UI_JSON} (legacy file preserved)"
        )
    finally:
        _GS_UI_MIGRATED = True

    return raw


def _migrate_api_from_scripting(
    raw: dict[str, Any],
    yaml_path: Path | None,
) -> dict[str, Any]:
    """Relocate the REST-API surface config out of the legacy ``scripting`` block.

    The host/port for the agent's main HTTP server and the optional
    Mission Control URL used to live under ``scripting.rest_api`` and
    ``scripting.mission_control_url``. They now live under the dedicated
    ``api`` section (``api.rest`` and ``api.mission_control_url``).

    Runs at most once per process (guarded by
    ``_API_FROM_SCRIPTING_MIGRATED``). Per-field check: a value already
    present under ``api`` wins and is never overwritten. On a live
    migration the resolved values are written into ``raw`` in-memory AND
    flushed back to the on-disk YAML so an operator who customized the
    REST port keeps it after the legacy block is dropped on load.
    """
    global _API_FROM_SCRIPTING_MIGRATED
    if _API_FROM_SCRIPTING_MIGRATED:
        return raw

    try:
        legacy = raw.get("scripting")
        if not isinstance(legacy, dict):
            _API_FROM_SCRIPTING_MIGRATED = True
            return raw

        legacy_rest = legacy.get("rest_api")
        legacy_mc_url = legacy.get("mission_control_url")
        if not isinstance(legacy_rest, dict) and legacy_mc_url is None:
            _API_FROM_SCRIPTING_MIGRATED = True
            return raw

        api_section = raw.get("api")
        if not isinstance(api_section, dict):
            api_section = {}

        merged_any = False

        if isinstance(legacy_rest, dict):
            rest_section = api_section.get("rest")
            if not isinstance(rest_section, dict):
                rest_section = {}
            for key in ("enabled", "host", "port"):
                if key in legacy_rest and key not in rest_section:
                    rest_section[key] = legacy_rest[key]
                    merged_any = True
            if rest_section:
                api_section["rest"] = rest_section

        if legacy_mc_url is not None and "mission_control_url" not in api_section:
            api_section["mission_control_url"] = legacy_mc_url
            merged_any = True

        if not merged_any:
            _API_FROM_SCRIPTING_MIGRATED = True
            return raw

        raw["api"] = api_section

        if yaml_path is not None:
            try:
                to_write: dict[str, Any] = {}
                if yaml_path.is_file():
                    with open(yaml_path, encoding="utf-8") as fh:
                        loaded = yaml.safe_load(fh)
                    if isinstance(loaded, dict):
                        to_write = loaded
                disk_api = to_write.get("api")
                if not isinstance(disk_api, dict):
                    disk_api = {}
                if "rest" in api_section and "rest" not in disk_api:
                    disk_api["rest"] = api_section["rest"]
                if (
                    "mission_control_url" in api_section
                    and "mission_control_url" not in disk_api
                ):
                    disk_api["mission_control_url"] = api_section[
                        "mission_control_url"
                    ]
                to_write["api"] = disk_api

                body = yaml.safe_dump(
                    to_write,
                    sort_keys=False,
                    default_flow_style=False,
                )
                yaml_path.parent.mkdir(parents=True, exist_ok=True)
                tmp_path = yaml_path.with_suffix(yaml_path.suffix + ".tmp")
                tmp_path.write_text(body, encoding="utf-8")
                _os.replace(str(tmp_path), str(yaml_path))
            except (OSError, yaml.YAMLError):
                pass

        import logging as _logging

        _logging.getLogger("ados.core.config").info(
            "migrated rest_api + mission_control_url into the api section "
            "(legacy scripting block ignored on load)"
        )
    finally:
        _API_FROM_SCRIPTING_MIGRATED = True

    return raw


def _deep_merge(base: dict[str, Any], override: dict[str, Any]) -> dict[str, Any]:
    """Merge override into base recursively."""
    merged = base.copy()
    for key, val in override.items():
        if key in merged and isinstance(merged[key], dict) and isinstance(val, dict):
            merged[key] = _deep_merge(merged[key], val)
        else:
            merged[key] = val
    return merged
