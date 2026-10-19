"""The ``sensors.hub`` config block."""

from __future__ import annotations

import pytest
from pydantic import ValidationError

from ados.core.config import ADOSConfig, SensorHubConfig


def test_defaults_match_the_native_reader():
    hub = ADOSConfig().sensors.hub
    assert hub.transport == "auto"
    assert hub.imu_odr_hz == 1000
    assert hub.trigger_hz == 20
    assert hub.exposure_us == 4000
    assert hub.gnss_forward is True


@pytest.mark.parametrize(
    "transport", ["auto", "serial:/dev/ttyACM0", "tcp:127.0.0.1:7600", "tcp:hub-sim:7601"]
)
def test_transport_accepts_the_three_forms(transport: str):
    assert SensorHubConfig(transport=transport).transport == transport


@pytest.mark.parametrize(
    "transport", ["", "serial:", "serial:ttyACM0", "tcp:host", "usb:0", "auto "]
)
def test_transport_rejects_anything_else(transport: str):
    with pytest.raises(ValidationError):
        SensorHubConfig(transport=transport)


def test_imu_rate_is_one_of_the_hub_rates():
    assert SensorHubConfig(imu_odr_hz=2000).imu_odr_hz == 2000
    with pytest.raises(ValidationError):
        SensorHubConfig(imu_odr_hz=800)
