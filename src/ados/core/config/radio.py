"""Radio-lane configuration (the top-level ``radio:`` section).

Holds the auxiliary RC-radio lanes that are separate from the WFB data link
(``video.wfb``) and from the FC serial link (``mavlink``). Today that is the
CRSF/ExpressLRS block; the section exists in the model so the pin survives a
full config save (``ADOSConfig.model_dump()`` rewrites the whole YAML, so an
unmodelled section would be silently dropped on the next write).
"""

from __future__ import annotations

from pydantic import BaseModel


class CrsfConfig(BaseModel):
    """The ExpressLRS / CRSF RC control lane.

    ``device`` pins the serial node the RC transmitter module is attached to
    (e.g. ``/dev/ttyUSB0``). A pinned device is excluded from flight-controller
    serial discovery and classified as the RC module by the hot-plug monitor,
    so plugging the module can never disturb the FC link. Empty = no pin.

    ``enabled`` opts the lane in. It additionally allows the hot-plug monitor
    to classify a known RC-bridge USB id (CP2102/CH340/ESP32-S3) as the RC
    module even without a pin; with the lane off and no pin, a generic
    USB-serial bridge stays an FC candidate (a VID alone cannot distinguish an
    FC behind the same bridge from an RC module).
    """

    enabled: bool = False
    device: str = ""


class RadioConfig(BaseModel):
    crsf: CrsfConfig = CrsfConfig()
