"""Universal setup status assembly and remote-access helpers.

The implementation now lives in per-concern files alongside this
barrel:

* ``_status.py`` — :func:`build_setup_status` orchestrator.
* ``_access_urls.py`` — access-URL composer + video-access probe +
  Mission Control URL chooser.
* ``_cloud_actions.py`` — Cloudflare token extract / install +
  cloud-posture read / apply.
* ``_net_helpers.py`` — hostname / local-IP discovery, MAVLink
  port selectors, Host-header validator.
* ``_service_inspection.py`` — service list + remote-access status.
* ``_constants.py`` — wire constants (hotspot / USB / MAVLink port).

Existing callers (``from ados.setup.service import X``) keep working
unchanged via the re-exports below.
"""

from __future__ import annotations

from ados.setup.state_machine import (  # re-export for callers/tests
    _resolve_display_step,
    _setup_steps,
    build_setup_steps,
)

from ._access_urls import (
    _access_urls,
    _dedupe_urls,
    _mission_control_url,
    _setup_path,
    _usb_setup_url,
    _video_access,
)
from ._cloud_actions import (
    _cloud_choice_status,
    apply_cloud_choice,
    extract_cloudflare_token,
    install_cloudflare_token,
)
from ._constants import (
    _HOTSPOT_IP,
    _HOTSPOT_URL,
    _TOKEN_RE,
    _USB_GADGET_IP,
    _USB_URL_TEMPLATE,
    DEFAULT_MAVLINK_TCP_PORT,
)
from ._net_helpers import (
    _best_lan_host,
    _build_known_hosts,
    _first_mavlink_tcp_port,
    _first_mavlink_ws_port,
    _hostname,
    _local_ips,
    _safe_host_for,
)
from ._service_inspection import (
    _cloudflared_running,
    _remote_status,
    _service_state,
    _services,
)
from ._status import build_setup_status

__all__ = [
    # public API
    "build_setup_status",
    "apply_cloud_choice",
    "install_cloudflare_token",
    "extract_cloudflare_token",
    "DEFAULT_MAVLINK_TCP_PORT",
    # state-machine re-exports
    "_resolve_display_step",
    "_setup_steps",
    "build_setup_steps",
    # net helpers
    "_hostname",
    "_local_ips",
    "_first_mavlink_ws_port",
    "_first_mavlink_tcp_port",
    "_best_lan_host",
    "_build_known_hosts",
    "_safe_host_for",
    # service inspection
    "_services",
    "_service_state",
    "_remote_status",
    "_cloudflared_running",
    # access urls + video
    "_video_access",
    "_mission_control_url",
    "_setup_path",
    "_usb_setup_url",
    "_access_urls",
    "_dedupe_urls",
    # cloud
    "_cloud_choice_status",
    # constants
    "_HOTSPOT_IP",
    "_HOTSPOT_URL",
    "_TOKEN_RE",
    "_USB_GADGET_IP",
    "_USB_URL_TEMPLATE",
]
