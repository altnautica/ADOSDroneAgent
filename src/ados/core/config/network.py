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


class MacPinConfig(BaseModel):
    # Auto-pin a stable MAC on an onboard adapter that has no efuse MAC and so
    # randomizes its address every boot (which churns the DHCP lease and the
    # box's IP). On by default: the auto path only writes a next-boot
    # systemd-networkd .link and never touches the live interface, so it cannot
    # drop a management link. The Rust installer step + supervisor reconciler
    # read this same field -- keep the default (true) in sync with them.
    enabled: bool = True
    # Re-tagging the LIVE interface (fixes the IP this session without a reboot)
    # drops any connection over that interface, so it stays opt-in.
    apply_live_allowed: bool = False
    # Operator overrides keyed by "vvvv:pppp" USB id or interface name -> MAC.
    overrides: dict[str, str] = {}


class WifiSelfHealConfig(BaseModel):
    # Reactive self-heal for the onboard management-WiFi data path. The radio
    # bring-up (global regulatory set + the injection adapter going into monitor
    # mode) can leave the onboard WiFi associated-but-dead: a strong link and a
    # valid IP yet no traffic (the gateway ARP never resolves). The supervisor
    # watchdog detects this and re-associates the connection so the box keeps a
    # working failover when its wired link is unplugged. On by default. It only
    # ever touches onboard managed WiFi, never the radio adapter or wired.
    enabled: bool = True
    # Consecutive failing checks before a re-association fires (a single failing
    # check can be a momentarily-busy gateway).
    fail_threshold: int = 2
    # Quiet period after a heal, per connection, so a re-association in progress
    # is never re-fired on (anti-flap).
    cooldown_s: int = 60


class NetworkConfig(BaseModel):
    wifi_client: WifiClientConfig = WifiClientConfig()
    cellular: CellularConfig = CellularConfig()
    hotspot: HotspotConfig = HotspotConfig()
    mac_pin: MacPinConfig = MacPinConfig()
    wifi_selfheal: WifiSelfHealConfig = WifiSelfHealConfig()
