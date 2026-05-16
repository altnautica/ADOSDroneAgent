"""Row registry binding row IDs to handlers and labels.

Holding the registry in its own module keeps the per-domain handler
files free of ordering knowledge. The :class:`SettingsPage` instance
imports ``ROW_DEFS`` and walks it for both render and hit-zone
calculations.
"""

from __future__ import annotations

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

ROW_DEFS: tuple[Row, ...] = (
    Row("network.hotspot", "Wi-Fi hotspot", "default", _wifi_hotspot_drilldown),
    Row("network.hotspot.on", "Hotspot enabled", "toggle", _hotspot_toggle),
    Row("network.wifi_client", "Wi-Fi client", "default", _wifi_client_drilldown),
    Row("wfb.channel", "Channel", "default", _channel_enum),
    Row("wfb.tx_power_dbm", "TX power", "default", _tx_power_slider),
    Row("wfb.mcs_index", "MCS index", "default", _mcs_enum),
    Row("wfb.topology", "Topology", "default", _topology_enum),
    Row("wfb.auto_pair", "Auto-pair", "toggle", _auto_pair_toggle),
    Row("ground.role", "Role", "default", _role_enum),
    Row("server.mode", "Cloud mode", "default", _cloud_mode_enum),
    Row("display.binding", "Display", "default", _display_drilldown),
    Row("display.calibrate", "Calibrate touch", "action", _calibrate_action),
    Row("display.rotation", "Display rotation", "default", _rotation_enum),
    Row("ui.theme", "Theme", "default", _theme_enum),
    Row("logging.level", "Log level", "default", _log_level_enum),
    Row("system.reboot", "Reboot now", "action", _reboot_action),
    Row("system.factory_reset", "Factory reset", "action", _factory_reset_action),
    Row("about", "About", "default", _about_drilldown),
)


__all__ = ["ROW_DEFS"]
