"""Sensor configuration (the ``sensors:`` block).

``sensors.hub`` is read only by the native ``ados-sensorhub`` service, which
streams the USB sensor hub's IMU, trigger, barometer, magnetometer and GNSS
packets onto the sensor bus. It is modelled here so its defaults are stated
once, in the schema every surface reads.
"""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field


class SensorHubConfig(BaseModel):
    """The sensor hub link and the configuration sent to the hub on connect.

    ``transport`` is ``auto`` (the first ``/dev/ttyACM*`` whose USB id is the
    hub firmware's ``1209:ad05``), ``serial:<device>`` or
    ``tcp:<host>:<port>`` (a simulated hub).
    """

    transport: str = Field(
        default="auto", pattern=r"^(auto|serial:/\S+|tcp:\S+:\d{1,5})$"
    )
    imu_odr_hz: Literal[500, 1000, 2000] = 1000
    # Camera trigger rate; 0 turns the trigger outputs off.
    trigger_hz: int = Field(default=20, ge=0, le=1000)
    # Commanded exposure, used for mid-exposure timing when no strobe is wired.
    exposure_us: int = Field(default=4000, ge=0, le=1_000_000)
    # Publish the hub's GNSS passthrough bytes on the sensor bus.
    gnss_forward: bool = True


class SensorsConfig(BaseModel):
    hub: SensorHubConfig = SensorHubConfig()
