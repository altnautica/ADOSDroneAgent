"""Shared constants for the setup service package.

Wire addresses for the local-access endpoints the agent itself binds
(hotspot AP and USB gadget) and the default MAVLink TCP port the
in-process proxy uses.
"""

from __future__ import annotations

import re

# Canonical local-access endpoints. These mirror the addresses configured by
# ``services.network.wifi_ap`` (hotspot AP) and ``services.ground_station.usb_gadget``
# (RNDIS / CDC-NCM USB tether). Keep in sync with those modules.
_HOTSPOT_IP = "192.168.4.1"
_USB_GADGET_IP = "192.168.7.1"
_HOTSPOT_URL = f"http://{_HOTSPOT_IP}"
_USB_URL_TEMPLATE = "http://{ip}:{port}"
_TOKEN_RE = re.compile(r"(?:--token|service\s+install)\s+['\"]?([^'\"\s]+)")

# Default port for the always-on MAVLink TCP proxy. The proxy is started
# unconditionally by ``ados.core.main.AgentApp`` with this hardcoded port
# (search ``TcpProxy(self._fc_connection, port=5760)``) and is NOT
# registered in ``config.mavlink.endpoints``. The helpers below fall back
# to this constant when the endpoints walk finds no TCP entry so the
# CLI and heartbeat surfaces always advertise the live listener.
# Keep this value locked with the TcpProxy instantiation.
DEFAULT_MAVLINK_TCP_PORT = 5760


__all__ = [
    "_HOTSPOT_IP",
    "_USB_GADGET_IP",
    "_HOTSPOT_URL",
    "_USB_URL_TEMPLATE",
    "_TOKEN_RE",
    "DEFAULT_MAVLINK_TCP_PORT",
]
