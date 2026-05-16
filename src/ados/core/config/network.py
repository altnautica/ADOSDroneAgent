"""Network configuration (WiFi client, cellular, hotspot)."""

from __future__ import annotations

from pydantic import BaseModel


class WifiClientConfig(BaseModel):
    enabled: bool = False
    ssid: str = ""
    password: str = ""


class CellularConfig(BaseModel):
    enabled: bool = False
    apn: str = ""


class HotspotConfig(BaseModel):
    enabled: bool = True
    ssid: str = "ADOS-{device_id}"
    # Default WPA2 passphrase used when the agent first brings up its own
    # access point. Predictable so operators can connect from a phone at
    # the bench without reading a generated value off disk. Override in
    # config.yaml for any deployment that needs a unique passphrase.
    password: str = "altnautica"
    channel: int = 6


class NetworkConfig(BaseModel):
    wifi_client: WifiClientConfig = WifiClientConfig()
    cellular: CellularConfig = CellularConfig()
    hotspot: HotspotConfig = HotspotConfig()
