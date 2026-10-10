"""The WFB adapter record the radio service reports.

Adapter detection, classification, monitor-mode setup, regulatory domain and
TX power are owned by the native radio service. Python reads its facts through
:mod:`ados.services.wfb.adapter_probe` (the radio's sidecar, else its one-shot
``--list-adapters`` scan) and holds them in this record.
"""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class WifiAdapterInfo:
    """Information about a detected WiFi adapter."""

    interface_name: str
    driver: str
    chipset: str
    supports_monitor: bool
    current_mode: str
    phy: str = ""
    usb_vid: int = 0
    usb_pid: int = 0
    is_wfb_compatible: bool = False
    capabilities: list[str] = field(default_factory=list)


__all__ = ["WifiAdapterInfo"]
