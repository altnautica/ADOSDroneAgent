"""Shared helpers, Pydantic models, and constants for ground-station routes.

Every sub-router module imports these helpers indirectly. Tests
monkeypatch them through ``ados.api.routes.ground_station.<name>``,
which is wired via the package ``__init__.py``. Sub-modules read the
helper at call time through the package object so monkeypatched
values take effect at the call site.

The implementation now lives in per-concern files alongside this
barrel:

* ``profile.py`` — profile gate + agent-config save helper.
* ``ui_config.py`` — UI / display config load + persist (JSON +
  YAML-backed) and the in-memory mirror helper.
* ``system_snapshot.py`` — CPU / RAM / temp / uptime / agent version.
* ``managers.py`` — lazy-import singletons for the live service
  managers (hostapd, pair).
* ``views.py`` — view aggregators that compose the live status JSON
  (radio link, AP-only network block, WFB, AP-guard diagnostics).
* ``models.py`` — Pydantic request models.
* ``validators.py`` — IPv4 helpers, AP-subnet gate, JSON / YAML
  file readers, stock confirm token.
* ``_paths.py`` — path constants and default-config blobs.

Existing callers
(``from ados.api.routes.ground_station._common import X``) keep
working unchanged via the re-exports below.
"""

from __future__ import annotations

from ._paths import (
    _DEFAULT_BUTTONS,
    _DEFAULT_DISPLAY,
    _DEFAULT_OLED,
    _DEFAULT_SCREENS,
    _MESH_STATE_JSON,
    _UI_CONFIG_PATH,
    _WFB_RECEIVER_JSON,
    _WFB_RELAY_JSON,
)
from .managers import _pair_manager
from .models import (
    BluetoothPairRequest,
    BluetoothScanRequest,
    ButtonsUpdate,
    GamepadPrimaryUpdate,
    MeshConfigUpdate,
    MeshGatewayPreferenceUpdate,
    OledUpdate,
    PairAcceptRequest,
    PairApproveRequest,
    PairJoinRequest,
    PairRequest,
    PairRevokeRequest,
    PicClaimRequest,
    PicConfirmTokenRequest,
    PicHeartbeatRequest,
    PicReleaseRequest,
    RoleChangeRequest,
    ScreensUpdate,
    WfbUpdate,
    WifiJoinRequest,
)
from .profile import _require_ground_profile, _save_config
from .system_snapshot import _agent_version, _system_snapshot
from .ui_config import (
    _load_display_config,
    _load_ui_config,
    _persist_gs_ui_section,
    _refresh_in_memory_ui,
    _save_ui_config,
)
from .validators import (
    _IPV4_RE,
    _is_ap_subnet_client,
    _read_json_or_empty,
    _read_yaml_or_empty,
    _stock_confirm_token,
    _validate_ipv4,
    _validate_ipv4_cidr,
)
from .views import _read_wfb_view

__all__ = [
    # paths + defaults
    "_UI_CONFIG_PATH",
    "_DEFAULT_OLED",
    "_DEFAULT_BUTTONS",
    "_DEFAULT_SCREENS",
    "_DEFAULT_DISPLAY",
    "_MESH_STATE_JSON",
    "_WFB_RELAY_JSON",
    "_WFB_RECEIVER_JSON",
    # profile
    "_require_ground_profile",
    "_save_config",
    # ui config
    "_load_ui_config",
    "_save_ui_config",
    "_load_display_config",
    "_persist_gs_ui_section",
    "_refresh_in_memory_ui",
    # system snapshot
    "_agent_version",
    "_system_snapshot",
    # managers
    "_pair_manager",
    # views
    "_read_wfb_view",
    # validators
    "_IPV4_RE",
    "_stock_confirm_token",
    "_validate_ipv4",
    "_validate_ipv4_cidr",
    "_is_ap_subnet_client",
    "_read_json_or_empty",
    "_read_yaml_or_empty",
    # models
    "WfbUpdate",
    "PairRequest",
    "OledUpdate",
    "ButtonsUpdate",
    "ScreensUpdate",
    "BluetoothScanRequest",
    "BluetoothPairRequest",
    "GamepadPrimaryUpdate",
    "PicClaimRequest",
    "PicReleaseRequest",
    "PicConfirmTokenRequest",
    "PicHeartbeatRequest",
    "WifiJoinRequest",
    "RoleChangeRequest",
    "MeshConfigUpdate",
    "MeshGatewayPreferenceUpdate",
    "PairAcceptRequest",
    "PairApproveRequest",
    "PairRevokeRequest",
    "PairJoinRequest",
]
