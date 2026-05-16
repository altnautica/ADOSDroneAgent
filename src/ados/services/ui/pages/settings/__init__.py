"""Settings page — scrollable list of editor rows.

Renders the Settings tab content area (480x244 minus the bottom tab
bar) as a vertically scrollable column of 48 px rows. Each row binds
a label + current value + handler. Tapping fires the handler, which
typically pushes a modal (enum picker / slider / keyboard / confirm
dialog) onto the navigator. On modal save, the handler issues the
matching REST call and updates the cached snapshot the rows draw
from.

The implementation now lives in per-domain files alongside this
barrel:

* ``page.py`` — :class:`SettingsPage` (lifecycle, scroll, render,
  dispatch).
* ``network.py`` — Wi-Fi hotspot SSID, hotspot toggle, Wi-Fi client.
* ``radio.py`` — WFB channel / TX power / MCS / topology / auto-pair
  plus the ground-station role selector.
* ``cloud_display.py`` — cloud posture and physical display rows.
* ``system.py`` — theme, log level, reboot, factory reset, about.
* ``_registry.py`` — :data:`ROW_DEFS` binding rows to handlers.
* ``_row.py`` — :class:`Row` dataclass.
* ``_common.py`` — shared constants and the apply-helper.

Existing callers (``from ados.services.ui.pages.settings import
SettingsPage``) keep working unchanged.
"""

from __future__ import annotations

from ._common import PAGE_H, PAGE_W, SNAPSHOT_TTL_S, _post_apply, _safe_dict
from ._registry import ROW_DEFS
from ._row import Row
from .cloud_display import (
    _calibrate_action,
    _cloud_mode_enum,
    _display_drilldown,
    _rotation_enum,
)
from .network import (
    _hotspot_toggle,
    _wifi_client_drilldown,
    _wifi_hotspot_drilldown,
)
from .page import SettingsPage
from .radio import (
    _auto_pair_toggle,
    _channel_enum,
    _mcs_enum,
    _role_enum,
    _topology_enum,
    _tx_power_slider,
)
from .system import (
    _about_drilldown,
    _factory_reset_action,
    _log_level_enum,
    _reboot_action,
    _theme_enum,
)

# Backward-compatible alias for the constant the old single-file module
# exposed via the lowercase ``_SNAPSHOT_TTL_S`` name.
_SNAPSHOT_TTL_S = SNAPSHOT_TTL_S
# Backward-compatible alias for the registry tuple under its old name.
_ROW_DEFS = ROW_DEFS

__all__ = [
    "PAGE_H",
    "PAGE_W",
    "ROW_DEFS",
    "Row",
    "SNAPSHOT_TTL_S",
    "SettingsPage",
    "_ROW_DEFS",
    "_SNAPSHOT_TTL_S",
    "_about_drilldown",
    "_auto_pair_toggle",
    "_calibrate_action",
    "_channel_enum",
    "_cloud_mode_enum",
    "_display_drilldown",
    "_factory_reset_action",
    "_hotspot_toggle",
    "_log_level_enum",
    "_mcs_enum",
    "_post_apply",
    "_reboot_action",
    "_role_enum",
    "_rotation_enum",
    "_safe_dict",
    "_theme_enum",
    "_topology_enum",
    "_tx_power_slider",
    "_wifi_client_drilldown",
    "_wifi_hotspot_drilldown",
]
