"""Ground-station profile services.

Mirror of the air-side video and wfb services, flipped to the receive
side. These modules run when AgentConfig.profile == "ground_station"
(or when profile_detect decides the node is a ground station).

Three managers live here:

- WfbRxManager: detects a WFB-ng compatible adapter, puts it in monitor
  mode, and runs `wfb_rx` to receive the encrypted radio stream from
  the drone and emit the decoded video on localhost UDP.
- MediamtxGsManager: runs mediamtx configured to ingest the wfb_rx
  localhost UDP feed and republish it as WHEP on :8889 for the browser
  GCS and any LAN client.
- UsbGadgetManager: builds a libcomposite CDC-NCM + RNDIS gadget on
  the Pi 4B USB-C OTG port, brings up usb0 at 192.168.7.1/24, and
  spawns a single-host dnsmasq so a tethered laptop auto-configures.

Each module is independently runnable via `python -m` for systemd.
"""

from __future__ import annotations

from ados.services.ground_station.mediamtx_manager import MediamtxGsManager
from ados.services.ground_station.usb_gadget import UsbGadgetManager
from ados.services.ground_station.wfb_rx import WfbRxManager

__all__ = ["MediamtxGsManager", "UsbGadgetManager", "WfbRxManager"]
