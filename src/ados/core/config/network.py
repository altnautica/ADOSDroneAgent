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
    # Off by default. The most common operator setup is "SBC is already
    # on the home WiFi network" -- in that case auto-binding an
    # additional access point on the same wlan0 interface is at best
    # confusing (the box ends up with two IPs on one NIC) and at worst
    # blocks the home-WiFi association. Operators who genuinely need
    # the hotspot opt in via the Setup webapp's Network step or by
    # setting network.hotspot.enabled=true in /etc/ados/config.yaml.
    enabled: bool = False
    ssid: str = "ADOS-{device_id}"
    # Default WPA2 passphrase used when the agent brings up its access
    # point. Predictable so operators can connect from a phone at the
    # bench without reading a generated value off disk. Override in
    # config.yaml for any deployment that needs a unique passphrase.
    password: str = "altnautica"
    channel: int = 6


class NetworkConfig(BaseModel):
    wifi_client: WifiClientConfig = WifiClientConfig()
    cellular: CellularConfig = CellularConfig()
    hotspot: HotspotConfig = HotspotConfig()
