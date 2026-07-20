"""UI config persistence helpers.

Two layers of UI state live in this module:

* ``_load_ui_config`` / ``_save_ui_config`` — the legacy JSON side-file
  consumed by the OLED service and exposed for backward compatibility.
* ``_persist_gs_ui_section`` / ``_refresh_in_memory_ui`` — the
  authoritative path that writes ``ground_station.ui.<section>`` into
  the YAML-backed ``ADOSConfig`` and mirrors it into the running
  service.

The display (HDMI kiosk) sub-config reads ``ground_station.kiosk`` from
the YAML-backed ``ADOSConfig`` — the single source of truth the kiosk
service and the native write route also use — via ``_load_display_config``.
Writes go through the native ``ados-control`` display route.
"""

from __future__ import annotations

import json
from typing import Any

from ._paths import (
    _DEFAULT_BUTTONS,
    _DEFAULT_DISPLAY,
    _DEFAULT_OLED,
    _DEFAULT_SCREENS,
    _UI_CONFIG_PATH,
)


def _load_ui_config() -> dict[str, Any]:
    """Load the UI config blob, filling any missing keys with defaults."""
    data: dict[str, Any] = {}
    try:
        if _UI_CONFIG_PATH.is_file():
            data = json.loads(_UI_CONFIG_PATH.read_text(encoding="utf-8")) or {}
    except (OSError, ValueError):
        data = {}

    oled = {**_DEFAULT_OLED, **(data.get("oled") or {})}
    buttons = {**_DEFAULT_BUTTONS, **(data.get("buttons") or {})}
    screens = {**_DEFAULT_SCREENS, **(data.get("screens") or {})}
    return {"oled": oled, "buttons": buttons, "screens": screens}


def _save_ui_config(data: dict[str, Any]) -> None:
    """Atomic write to the UI config file. Best effort; errors surface as 500."""
    _UI_CONFIG_PATH.parent.mkdir(parents=True, exist_ok=True)
    tmp = _UI_CONFIG_PATH.with_suffix(".tmp")
    tmp.write_text(json.dumps(data, indent=2, sort_keys=True), encoding="utf-8")
    tmp.replace(_UI_CONFIG_PATH)


def _load_display_config() -> dict[str, Any]:
    """Project the persisted kiosk config into the display-route wire shape.

    Reads ``ground_station.kiosk`` from ``/etc/ados/config.yaml`` (the single
    source of truth the kiosk service also reads) and maps the config fields
    (``enabled`` / ``resolution`` / ``target_url``) onto the display route's
    ``{resolution, kiosk_enabled, kiosk_target_url}`` contract, over the
    built-in defaults. An absent / unreadable config yields the all-defaults
    shape. The native ``ados-control`` read route projects the same section
    identically.
    """
    from ados.services.ground_station.pair_manager import _load_config_dict

    data = _load_config_dict()
    gs = data.get("ground_station") if isinstance(data, dict) else None
    kiosk = gs.get("kiosk") if isinstance(gs, dict) else None
    if not isinstance(kiosk, dict):
        kiosk = {}

    display = dict(_DEFAULT_DISPLAY)
    if "resolution" in kiosk:
        display["resolution"] = kiosk["resolution"]
    if "enabled" in kiosk:
        display["kiosk_enabled"] = kiosk["enabled"]
    if "target_url" in kiosk:
        display["kiosk_target_url"] = kiosk["target_url"]
    return display


def _persist_gs_ui_section(section: str, value: dict[str, Any]) -> None:
    """Write ``ground_station.ui.<section>`` into the YAML-backed ADOSConfig.

    The OLED, button, and screen UI config lives in the Pydantic model
    so it round-trips through save cycles and is consumed by the live
    services. The legacy JSON side-file is no longer written, but
    remains on disk for rollback (the load-time migrator preserves it).
    """
    from ados.services.ground_station.pair_manager import (
        _load_config_dict,
        _save_config_dict,
    )

    data = _load_config_dict()
    gs_section = data.get("ground_station")
    if not isinstance(gs_section, dict):
        gs_section = {}
        data["ground_station"] = gs_section
    ui_section = gs_section.get("ui")
    if not isinstance(ui_section, dict):
        ui_section = {}
        gs_section["ui"] = ui_section
    ui_section[section] = value
    if not _save_config_dict(data):
        raise OSError("failed to persist ground_station.ui to /etc/ados/config.yaml")


def _refresh_in_memory_ui(app: Any, section: str, value: dict[str, Any]) -> None:
    """Mirror the persisted section into the running app config."""
    try:
        gs = getattr(app.config, "ground_station", None)
        if gs is None:
            return
        ui = getattr(gs, "ui", None)
        if ui is None:
            return
        if hasattr(ui, section):
            setattr(ui, section, dict(value))
    except Exception:
        pass


__all__ = [
    "_load_ui_config",
    "_save_ui_config",
    "_load_display_config",
    "_persist_gs_ui_section",
    "_refresh_in_memory_ui",
]
