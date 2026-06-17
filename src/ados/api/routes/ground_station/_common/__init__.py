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
  managers (hostapd, pair, ethernet, wifi-client, modem, uplink
  router, input, PIC arbiter).
* ``views.py`` — view aggregators that compose the live status JSON
  (link, network, AP, wifi-client, ethernet, modem, router state).
* ``share_uplink.py`` — read / persist / apply for the share-uplink
  toggle.
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
    _UPLINK_PRIORITY_PATH,
    _WFB_RECEIVER_JSON,
    _WFB_RELAY_JSON,
)
from .managers import (
    _ethernet_mgr,
    _hostapd_manager,
    _modem_mgr,
    _pair_manager,
    _uplink_router,
    _wifi_client_manager,
)
from .models import (
    ApUpdate,
    BluetoothPairRequest,
    BluetoothScanRequest,
    ButtonsUpdate,
    DisplayUpdate,
    EthernetConfigUpdate,
    GamepadPrimaryUpdate,
    MeshConfigUpdate,
    MeshGatewayPreferenceUpdate,
    ModemConfigUpdate,
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
    ShareUplinkUpdate,
    UplinkPriorityUpdate,
    WfbUpdate,
    WifiJoinRequest,
)
from .profile import _require_ground_profile, _save_config
from .share_uplink import (
    _apply_share_uplink,
    _load_share_uplink_flag,
    _persist_share_uplink_flag,
)
from .system_snapshot import _agent_version, _system_snapshot
from .ui_config import (
    _load_display_config,
    _load_ui_config,
    _persist_gs_ui_section,
    _refresh_in_memory_ui,
    _save_display_config,
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
from .views import (
    _ap_view,
    _ethernet_view,
    _link_view,
    _modem_view,
    _network_view,
    _read_wfb_view,
    _router_state_view,
    _wifi_client_view,
)

__all__ = [
    # paths + defaults
    "_UI_CONFIG_PATH",
    "_DEFAULT_OLED",
    "_DEFAULT_BUTTONS",
    "_DEFAULT_SCREENS",
    "_DEFAULT_DISPLAY",
    "_UPLINK_PRIORITY_PATH",
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
    "_save_display_config",
    "_persist_gs_ui_section",
    "_refresh_in_memory_ui",
    # system snapshot
    "_agent_version",
    "_system_snapshot",
    # managers
    "_hostapd_manager",
    "_pair_manager",
    "_ethernet_mgr",
    "_wifi_client_manager",
    "_modem_mgr",
    "_uplink_router",
    # views
    "_link_view",
    "_network_view",
    "_read_wfb_view",
    "_ap_view",
    "_wifi_client_view",
    "_ethernet_view",
    "_modem_view",
    "_router_state_view",
    # share uplink
    "_load_share_uplink_flag",
    "_persist_share_uplink_flag",
    "_apply_share_uplink",
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
    "ApUpdate",
    "PairRequest",
    "OledUpdate",
    "ButtonsUpdate",
    "ScreensUpdate",
    "DisplayUpdate",
    "BluetoothScanRequest",
    "BluetoothPairRequest",
    "GamepadPrimaryUpdate",
    "PicClaimRequest",
    "PicReleaseRequest",
    "PicConfirmTokenRequest",
    "PicHeartbeatRequest",
    "WifiJoinRequest",
    "ModemConfigUpdate",
    "UplinkPriorityUpdate",
    "ShareUplinkUpdate",
    "RoleChangeRequest",
    "MeshConfigUpdate",
    "MeshGatewayPreferenceUpdate",
    "PairAcceptRequest",
    "PairApproveRequest",
    "PairRevokeRequest",
    "PairJoinRequest",
    "EthernetConfigUpdate",
]
