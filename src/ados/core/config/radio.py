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
    serial discovery and classified as the RC module by the hot-plug monitor,
    so plugging the module can never disturb the FC link. ``None`` = no pin;
    the lane probes for a known RC-bridge when enabled.

    ``enabled`` opts the lane in. It additionally allows the hot-plug monitor
    to classify a known RC-bridge USB id (CP2102/CH340/ESP32-S3) as the RC
    module even without a pin; with the lane off and no pin, a generic
    USB-serial bridge stays an FC candidate (a VID alone cannot distinguish an
    FC behind the same bridge from an RC module).

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

    ``mode`` selects what the attached module carries: ``crsf_rc`` (the RC
    channel lane this service transmits), ``mavlink`` (the module runs its
    native MAVLink mode and is owned by the MAVLink router's serial/UDP
    source, not this lane), or ``airport`` (a generic serial data pipe).

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
    relay_role: Literal["none", "repeater", "agent_last_mile"] = "none"


class RadioConfig(BaseModel):
    crsf: CrsfConfig = CrsfConfig()
