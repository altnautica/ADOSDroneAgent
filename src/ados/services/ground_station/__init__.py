"""Ground-station profile services.

Mirror of the air-side video and wfb services, flipped to the receive
side. These modules run when AgentConfig.profile == "ground_station"
(or when profile_detect decides the node is a ground station).

Two managers live here:

- MediamtxGsManager: runs mediamtx configured to ingest the native
  receive plane's localhost UDP feed and republish it as WHEP on :8889
  for the browser GCS and any LAN client.
- UsbGadgetManager: builds a libcomposite CDC-NCM + RNDIS gadget on
  the Pi 4B USB-C OTG port, brings up usb0 at 192.168.7.1/24, and
  spawns a single-host dnsmasq so a tethered laptop auto-configures.

The direct-role receive plane runs the native ``ados-groundlink`` binary;
the mesh relay and receiver roles still ship a packaged Python module.
Each module here is independently runnable via `python -m` for systemd.
"""

from __future__ import annotations

from ados.services.ground_station.mediamtx_manager import MediamtxGsManager
from ados.services.ground_station.usb_gadget import UsbGadgetManager

__all__ = ["MediamtxGsManager", "UsbGadgetManager"]
