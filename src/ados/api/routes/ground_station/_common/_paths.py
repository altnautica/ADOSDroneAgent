"""Path constants and default-config blobs shared across helper modules.

Holding these in one tiny module avoids circular imports between the
helpers (``ui_config``, ``views``, ``share_uplink``) that all need
the same set of paths and defaults.
"""

from __future__ import annotations

from typing import Any

from ados.core.paths import (
    GS_UI_JSON,
    GS_UPLINK_JSON,
    MESH_STATE_JSON,
    WFB_RECEIVER_JSON,
    WFB_RELAY_JSON,
)

# Persistent UI config lives in a separate JSON file because the
# current Pydantic ADOSConfig does not model a ground_station section
# yet. Single file, atomic write, 0644.
_UI_CONFIG_PATH = GS_UI_JSON

# Server and OLED both use 0-255 native scale. 204 is roughly 80 percent
# of 255.
_DEFAULT_OLED: dict[str, Any] = {
    "brightness": 204,
    "auto_dim_enabled": True,
    "screen_cycle_seconds": 5,
}

_DEFAULT_BUTTONS: dict[str, Any] = {
    "mapping": {
        "B1_short": "cycle_screen",
        "B1_long": "toggle_backlight",
        "B2_short": "show_network",
        "B2_long": "show_qr",
        "B3_short": "confirm",
        "B3_long": "pair_drone",
    }
}

_DEFAULT_SCREENS: dict[str, Any] = {
    "order": ["home", "link", "drone", "network", "system", "qr"],
    "enabled": ["home", "link", "drone", "network", "system", "qr"],
}

_DEFAULT_DISPLAY: dict[str, Any] = {
    "resolution": "auto",
    "kiosk_enabled": False,
    "kiosk_target_url": None,
}

_UPLINK_PRIORITY_PATH = GS_UPLINK_JSON
_MESH_STATE_JSON = MESH_STATE_JSON
_WFB_RELAY_JSON = WFB_RELAY_JSON
_WFB_RECEIVER_JSON = WFB_RECEIVER_JSON


__all__ = [
    "_UI_CONFIG_PATH",
    "_DEFAULT_OLED",
    "_DEFAULT_BUTTONS",
    "_DEFAULT_SCREENS",
    "_DEFAULT_DISPLAY",
    "_UPLINK_PRIORITY_PATH",
    "_MESH_STATE_JSON",
    "_WFB_RELAY_JSON",
    "_WFB_RECEIVER_JSON",
]
