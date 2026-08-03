"""Network configuration (WiFi client, cellular, hotspot)."""

from __future__ import annotations

from typing import Literal

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
    # WPA2 passphrase for the agent's access point. Empty by default, which
    # makes the agent generate a per-unit passphrase on first use and store it
    # 0600 at /etc/ados/ap-passphrase.
    #
    # This used to default to a single predictable string so an operator could
    # connect from a phone without reading a generated value off disk. That
    # convenience is no longer needed -- the generated value is shown on the
    # OLED access-point page, in the on-box console, and on the installer's
    # completion card -- and a configured value takes precedence over the
    # generated one, so a non-empty default silently disabled per-unit
    # generation and gave every unit ever shipped the same key.
    #
    # Set this in config.yaml only to pin a deliberate shared passphrase.
    password: str = ""
    channel: int = 6
    # Which radio the access point binds. Empty means "resolve it by driver",
    # which is what the agent does -- interface names are not stable across
    # boots, so naming one here pins the AP to whatever that name happens to
    # mean on the next boot. Set it only to override the resolution.
    #
    # A REST read route already reported this key while nothing consumed it, so
    # a value set here used to be echoed back to the operator and then ignored.
    interface: str = ""


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


class RegulatoryConfig(BaseModel):
    # Operating-region posture for the radio (WFB) and any long-range
    # link. This is the single operator knob that decides whether the
    # agent radiates out of the box or first verifies an operating region.
    #
    #   unrestricted (default): the radio brings up and transmits on the
    #     configured home channel at the hardware-bounded power budget
    #     without first verifying a regional domain. The operator is
    #     responsible for legal RF operation in their jurisdiction. The
    #     state is surfaced honestly (an "unrestricted" badge) on every
    #     status surface; it is never silent.
    #   region: the operator has pinned an operating region (an
    #     ISO 3166-1 alpha-2 country code in ``region``). The radio
    #     applies that region's channel set and per-channel power limit
    #     before transmitting, and refuses to radiate on a channel the
    #     region forbids.
    #
    # This setting governs only the LEGAL/regulatory cap. The power-budget
    # and brownout clamps (video.wfb.tx_power_dbm / tx_power_max_dbm and
    # the TX-power ramp) and the link-liveness checks are always in force
    # regardless of mode.
    mode: Literal["unrestricted", "region"] = "unrestricted"
    # ISO 3166-1 alpha-2 country code (uppercase, e.g. "US", "DE", "GB")
    # when ``mode`` is "region". None when unrestricted.
    region: str | None = None
    # Operator id recorded when the region/posture was chosen (audit).
    ack_operator: str | None = None
    # ISO-8601 timestamp recorded when the region/posture was chosen
    # (audit). Stored as a plain string.
    ack_at: str | None = None


class NetworkConfig(BaseModel):
    wifi_client: WifiClientConfig = WifiClientConfig()
    cellular: CellularConfig = CellularConfig()
    hotspot: HotspotConfig = HotspotConfig()
    mac_pin: MacPinConfig = MacPinConfig()
    wifi_selfheal: WifiSelfHealConfig = WifiSelfHealConfig()
    regulatory: RegulatoryConfig = RegulatoryConfig()
