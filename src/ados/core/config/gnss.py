"""GNSS receivers, RTK base and moving-baseline heading (the ``gnss:`` block).

``ados-gnss`` reads this block. An empty ``receivers`` list leaves the service
idle. ``source`` names where a receiver's bytes come from:

* ``serial:/dev/ttyUSB0`` or ``serial:/dev/ttyUSB0@115200`` -- a serial port;
* ``udp:7800`` -- UDP datagrams on that local port (each datagram carries
  receiver bytes; configuration goes back to the latest sender), or
  ``udp:7800@192.168.1.50:7801`` to send configuration to a fixed address;
* ``hub`` -- the sensor hub's GNSS passthrough on the sensor bus (read only:
  the receiver cannot be configured through the hub).

``port`` is the receiver-side port the host is attached to; the RTK base
enables its RTCM3 output on that port.
"""

from __future__ import annotations

import math
import re
from typing import Literal

from pydantic import BaseModel, Field, field_validator, model_validator

_SOURCE_RE = re.compile(
    r"^(hub|udp:(\d{1,5})(@\d{1,3}(\.\d{1,3}){3}:\d{1,5})?|serial:[^@\s]+(@\d+)?)$"
)


class GnssReceiverConfig(BaseModel):
    """One GNSS receiver attached to this node."""

    id: str = Field(default="gnss0", pattern=r"^[a-z0-9_]+$")
    source: str = "hub"
    model: str | None = None
    port: Literal["uart1", "usb"] = "uart1"

    @field_validator("source")
    @classmethod
    def _source_shape(cls, v: str) -> str:
        m = _SOURCE_RE.match(v)
        if not m:
            raise ValueError(
                "gnss receiver source must be 'hub', 'udp:<port>[@<ipv4>:<port>]' "
                "or 'serial:<device>[@<baud>]'"
            )
        if v.startswith("udp:"):
            ports = [int(m.group(2))]
            if m.group(3):
                ports.append(int(m.group(3).rsplit(":", 1)[1]))
            if not all(1 <= p <= 65535 for p in ports):
                raise ValueError("gnss receiver udp ports must be 1-65535")
        return v


class GnssBaseConfig(BaseModel):
    """RTK base station mode of the first receiver."""

    mode: Literal["off", "survey", "fixed"] = "off"
    min_dur_s: int = Field(default=300, ge=1)
    accuracy_m: float = Field(default=2.0, gt=0)
    lat_deg: float | None = Field(default=None, ge=-90, le=90)
    lon_deg: float | None = Field(default=None, ge=-180, le=180)
    h_ellipsoid_m: float | None = None

    @model_validator(mode="after")
    def _fixed_needs_position(self) -> GnssBaseConfig:
        if self.mode == "fixed":
            h = self.h_ellipsoid_m
            no_pos = self.lat_deg is None or self.lon_deg is None
            if no_pos or h is None or not math.isfinite(h):
                raise ValueError(
                    "gnss.base mode 'fixed' needs lat_deg, lon_deg and h_ellipsoid_m"
                )
        return self


class GnssHeadingConfig(BaseModel):
    """Moving-baseline heading from two receivers."""

    enabled: bool = False


class GnssConfig(BaseModel):
    # A literal default (pydantic copies it) so the schema states it.
    receivers: list[GnssReceiverConfig] = []
    pps_device: str | None = None
    base: GnssBaseConfig = Field(default_factory=GnssBaseConfig)
    heading: GnssHeadingConfig = Field(default_factory=GnssHeadingConfig)

    @model_validator(mode="after")
    def _unique_ids(self) -> GnssConfig:
        ids = [r.id for r in self.receivers]
        if len(ids) != len(set(ids)):
            raise ValueError("gnss receiver ids must be unique")
        return self
