"""Radio-lane configuration (the top-level ``radio:`` section).

Holds the auxiliary RC-radio lanes that are separate from the WFB data link
(``video.wfb``) and from the FC serial link (``mavlink``). Today that is the
CRSF/ExpressLRS block; the section exists in the model so the pin survives a
full config save (``ADOSConfig.model_dump()`` rewrites the whole YAML, so an
unmodelled section would be silently dropped on the next write).
"""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel


class CrsfConfig(BaseModel):
    """The ExpressLRS / CRSF RC control lane.

    ``device`` pins the serial node the RC transmitter module is attached to
    (e.g. ``/dev/ttyUSB0``). A pinned device is excluded from flight-controller
    serial discovery and (in ``crsf_rc`` / ``airport`` mode) owned by the RC
    lane, so plugging the module can never disturb the FC link. ``None`` = no
    pin, and the lane stands by unconfigured: a device must be pinned for the
    lane to run — it never auto-probes for a bridge.

    ``enabled`` opts the lane in. It is what makes the pinned ``device`` an
    ADOS radio-lane port: while the lane is off the pin names nothing this
    agent owns, so flight-controller discovery treats every serial node — the
    pinned one included — as an FC candidate. Node classification is by the pin
    and the lane ``mode`` alone, never by a USB bridge id: a bridge VID
    (CP2102/CH340/ESP32-S3) cannot tell an FC behind that bridge apart from an
    RC module, so only the pinned device is ever taken for the RC module.

    ``band`` selects the module's operating band class: ``dual`` (a dual-band
    Gemini module running both links), ``900`` (sub-GHz only), or ``2p4``
    (2.4 GHz only). A target for the module, surfaced on the lane's status;
    the module's own parameter system owns the actual band change.

    ``packet_rate_hz`` is the RC frame cadence the lane transmits at (the
    protocol supports 50 to 500 frames per second; the service clamps an
    out-of-range value and logs it).

    ``tx_power_dbm`` is the requested conducted TX power for the module.
    ``None`` (the default) leaves the module at its own configured default —
    the lane never fabricates a power figure; the measured power comes back
    on link-statistics telemetry.

    ``mode`` selects what the attached module carries — and who owns its
    port: ``crsf_rc`` (the RC channel lane this service transmits; the
    MAVLink router excludes the pin from FC candidacy), ``mavlink`` (the
    module runs its native MAVLink mode — the module firmware owns the CRSF
    air protocol internally — and the MAVLink router ingests the carrier as
    its FC source: the pinned ``device`` at the fixed MAVLink-mode baud of
    460800 when ``mavlink_transport`` is ``serial``, or a UDP listen on port
    14550 for ``backpack_wifi``; the RC lane holds off the port entirely and
    stands by at state ``ready`` with the mode reported. By default this
    source is telemetry-up / command-down gated — the router reads inbound
    MAVLink so the drone appears and telemetry flows, but transmits nothing
    toward the FC over the RC lane until ``mavlink_command_enabled`` is set;
    see that field), or ``airport`` (a generic serial data pipe with no ADOS
    owner yet; the lane reads ``disabled``).

    ``mavlink_command_enabled`` opens the host-to-FC command direction for
    ``mode: mavlink``. It is OFF by default and is NOT implied by opting the
    lane in or selecting ``mode: mavlink``: a fresh MAVLink-over-ELRS source
    ingests telemetry only, with the command-down direction gated closed,
    until an operator explicitly sets this for a bench-validated command
    lane. It applies solely to the MAVLink-over-ELRS source and never affects
    any other flight-controller link.

    ``channel_source`` decides what feeds the transmitted channels in
    ``crsf_rc`` mode: ``hid`` (the handset/gamepad path only — the default),
    ``inject`` (the programmatic API only), or ``hybrid`` (both, with the PIC
    arbiter's holder deciding authority).

    ``mavlink_transport`` selects the carrier for ``mode: mavlink``:
    ``serial`` (the module's USB-serial port) or ``backpack_wifi`` (the
    module's WiFi backpack UDP bridge).

    ``relay_role`` declares this node's part in an RC relay chain: ``none``
    (not relaying — the default), ``repeater`` (a pure CRSF repeater), or
    ``agent_last_mile`` (an agent relay driving the ELRS last mile).
    """

    enabled: bool = False
    device: str | None = None
    band: Literal["dual", "900", "2p4"] = "dual"
    packet_rate_hz: int = 150
    tx_power_dbm: int | None = None
    mode: Literal["crsf_rc", "mavlink", "airport"] = "crsf_rc"
    channel_source: Literal["hid", "inject", "hybrid"] = "hid"
    mavlink_transport: Literal["serial", "backpack_wifi"] = "serial"
    mavlink_command_enabled: bool = False
    relay_role: Literal["none", "repeater", "agent_last_mile"] = "none"


class TunnelConfig(BaseModel):
    """The config-over-radio channel (a MAVLink-TUNNEL request/response lane on
    the low-rate WFB ``-p1`` control plane).

    It lets a node reachable only over the radio link (WFB carries no IP) have
    its ``/api/config`` read and written from the ground. It carries CONFIG
    request/response ONLY — never armed-flight command authority (a separate,
    gated concern) — and its only gate by riding ``-p1`` is the WFB pairing key
    (a pairing-scope gate, not a flight-authorization gate).

    ``enabled`` is the master opt-in. Off by default: the whole channel is
    inert — the drone-side terminator acts on no config tunnel and the
    ground-side injector refuses every request. It mirrors onto the
    ``/etc/ados/tunnel-enabled`` marker the ``ados-tunnel-config`` unit gates
    on, so an un-opted node never even runs the unit.

    ``command_enabled`` opens config WRITES (``PUT /api/config``) over the
    radio. Off by default and NOT implied by opting the channel in: a config
    READ (``GET``) is served while writes are refused until an operator sets
    this for a bench-validated write lane, after a safety review.

    ``rx_port`` / ``tx_port`` are the LOCAL UDP ports the service binds/sends
    on. They are dedicated ports (disjoint from the WFB plane ports) that an
    ``ados-radio`` bearer bridge connects to the ``-p1`` control plane in a
    separate, gated radio-integration step.
    """

    enabled: bool = False
    command_enabled: bool = False
    rx_port: int = 5820
    tx_port: int = 5821


class RadioConfig(BaseModel):
    crsf: CrsfConfig = CrsfConfig()
    tunnel: TunnelConfig = TunnelConfig()
