"""Ground-station profile configuration (UI, WFB roles, mesh)."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

from ados.core.paths import MESH_PSK_PATH

# ground_station fields live in the Pydantic model so they validate,
# round-trip through save cycles, and show up in config dumps. An earlier
# layout wrote `paired_drone_id` and `paired_at` to `/etc/ados/config.yaml`
# via direct YAML manipulation in pair_manager.py while ADOSConfig had
# `extra="ignore"`, and `share_uplink` lived in a side-file at
# `/etc/ados/ground-station-ui.json`. The migrator in `load_config()` picks
# the legacy side-file value up once and preserves the file on disk.


class GroundStationUiConfig(BaseModel):
    """OLED + buttons + screens UI config for the ground-station profile.

    Pulled out of the legacy `/etc/ados/ground-station-ui.json` side-file
    into the Pydantic model so it round-trips through save cycles and is
    consumed live by the display service and button_service. The legacy file is
    migrated once at load time and preserved on disk for rollback.

    Field shapes are intentionally loose (`dict`) because the OLED, button
    mapping, and screen order schemas are still evolving. The REST handlers
    and services know the keys they care about.
    """

    oled: dict = Field(default_factory=dict)
    buttons: dict = Field(default_factory=dict)
    screens: dict = Field(default_factory=dict)


class KioskConfig(BaseModel):
    """HDMI kiosk (Chromium-under-cage) configuration.

    The single source of truth for the kiosk. The kiosk service reads
    ``target_url`` (the page it points the browser at) and ``minimal_layer``
    (append ``?layer=minimal`` on low-RAM boards); the ``PUT
    /api/v1/ground-station/display`` write route and ``PUT /api/config``
    persist ``enabled`` / ``resolution`` / ``target_url`` here. Living in the
    Pydantic model means it validates, round-trips through save cycles, shows
    up in config dumps, and is read from exactly one place instead of the old
    ``ground-station-ui.json`` side-file the kiosk service never read.
    """

    enabled: bool = False
    resolution: str = "auto"
    target_url: str | None = None
    minimal_layer: bool = False


class WfbRelayConfig(BaseModel):
    """Relay-role WFB forwarder settings.

    On `relay` nodes, `wfb_rx -f` resolves the receiver via mDNS on the
    batman-adv interface and forwards fragments to its UDP listen port.
    """

    receiver_mdns_service: str = "_ados-receiver._tcp"
    receiver_port: int = 5800


class WfbReceiverConfig(BaseModel):
    """Receiver-role WFB aggregator settings.

    On `receiver` nodes, `wfb_rx -a` listens on `listen_port` for relay
    forwards and FEC-combines them with a local adapter stream if
    `accept_local_nic` is true.
    """

    listen_port: int = 5800
    accept_local_nic: bool = True


DisplayType = Literal["auto", "hdmi", "lcd", "none"]


class DisplayConfig(BaseModel):
    """Effective local-display primary path for the ground-station profile.

    ``type`` is the operator-set selection. ``auto`` lets the agent pick
    the strongest local renderer at runtime (HDMI > SPI LCD > none).
    Explicit values short-circuit detection: ``hdmi`` keeps the OLED /
    SPI LCD services from grabbing the panel on boards where the
    operator wants the Chromium kiosk to own the screen, ``lcd`` keeps
    the kiosk off boards that boot with both an HDMI sink and an SPI
    LCD wired, ``none`` disables all local-display services entirely
    (headless deployment).

    ``detected_type`` is runtime-only — it is populated by the heartbeat
    enrichment helper after probing what the OS actually exposes and is
    surfaced to Mission Control as a read-only field. Operators should
    not write to it via the config API.
    """

    type: DisplayType = "auto"
    detected_type: DisplayType | None = None


class MeshConfig(BaseModel):
    """batman-adv local mesh transport settings.

    `carrier` is the L2 technology on the second USB WiFi dongle:
    802.11s (native mesh, preferred) or IBSS (ad-hoc, fallback for
    drivers without 802.11s). `mesh_id` and `shared_key_path` gate the
    deployment so two adjacent sites on the same channel stay isolated.
    `shared_key_path` defaults to a device-derived key written on first
    boot by mesh_manager (0o600, never logged).
    """

    interface_override: str | None = None
    carrier: Literal["802.11s", "ibss"] = "802.11s"
    mesh_id: str | None = None
    shared_key_path: str = str(MESH_PSK_PATH)
    channel: int = 1  # 2.4 GHz ch 1 default for mesh dongle
    bat_iface: str = "bat0"


class GroundStationConfig(BaseModel):
    share_uplink: bool = False
    paired_drone_id: str | None = None
    paired_at: str | None = None  # iso timestamp
    ui: GroundStationUiConfig = Field(default_factory=GroundStationUiConfig)
    # gate the ground-station cloud relay's live state IPC read so a quick
    # rollback to the stub VehicleState is possible if the wiring causes
    # regressions in the field. Default True.
    use_live_state_ipc: bool = True

    # distributed receive role. `direct` is the single-node path and runs
    # wfb_rx the way a standalone ground station does. `relay` forwards
    # WFB fragments to a receiver over batman-adv. `receiver` aggregates
    # fragments from the local NIC and from remote relays and publishes
    # the combined FEC-repaired stream for the mediamtx pipeline.
    role: Literal["direct", "relay", "receiver"] = "direct"
    # Whether this node should advertise its uplink as a batman-adv
    # gateway. `auto` lets the mesh_manager decide based on actual
    # uplink health.
    cloud_uplink: Literal["auto", "force_on", "force_off"] = "auto"
    wfb_relay: WfbRelayConfig = WfbRelayConfig()
    wfb_receiver: WfbReceiverConfig = WfbReceiverConfig()
    mesh: MeshConfig = MeshConfig()
    # Operator-set primary local-display path. The OLED service gates
    # its early startup on this value so a board with HDMI + SPI LCD
    # wired together can be steered to one renderer without ripping
    # cables. The heartbeat enrichment helper resolves the effective
    # type (probing HDMI / framebuffer when set to "auto") and ships it
    # to Mission Control under ``displayType``.
    display: DisplayConfig = Field(default_factory=DisplayConfig)
    # HDMI kiosk config: the page the Chromium-under-cage kiosk points at plus
    # its render/display knobs. The kiosk service reads target_url + minimal_layer
    # here, and the display write route + PUT /api/config persist here, so the
    # kiosk config lives in exactly one place.
    kiosk: KioskConfig = Field(default_factory=KioskConfig)
