"""Board profile GPIO, variant and sensor binding models."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field


class GpioOutput(BaseModel):
    """One GPIO output line a board exposes (a buzzer, a status LED).

    ``pin`` is the BCM/SoC-native pin number the service drives. ``function``
    names the purpose (``buzzer``, ``led``) so a plugin can look up its pin by
    role instead of hardcoding one (operating rule: no hardcoded pins)."""

    id: str
    pin: int
    function: str


class GpioButton(BaseModel):
    """One front-panel button input a board declares (BCM numbering).

    A declaration says where the board's button wiring goes, not that a
    button is fitted: an unconnected pin with a pull-up reads the same as an
    idle button."""

    id: str
    pin: int
    header_pin: int | None = None
    function: str = ""


class ButtonPins(BaseModel):
    """The panel button roles a board wires, by the board's GPIO number: BCM on
    a Raspberry Pi, the Linux GPIO number (``bank * 32 + group * 8 + index``)
    on Rockchip. Optional: a board without the ``buttons:`` block has no panel
    buttons (a Raspberry Pi keeps the reference-panel lines)."""

    up: int = Field(ge=0)
    down: int = Field(ge=0)
    select: int = Field(ge=0)
    back: int = Field(ge=0)

    def pins(self) -> list[int]:
        """The four pins in role order (up, down, select, back)."""
        return [self.up, self.down, self.select, self.back]


class EncoderPins(BaseModel):
    """The rotary encoder (KY-040 class) a board wires, by the same GPIO
    numbering as ``buttons``: quadrature outputs ``a`` (CLK) and ``b`` (DT) and
    the push switch ``sw``. Optional: a board without the ``encoder:`` block has
    no encoder."""

    a: int = Field(ge=0)
    b: int = Field(ge=0)
    sw: int = Field(ge=0)

    def pins(self) -> list[int]:
        """The three pins in role order (a, b, sw)."""
        return [self.a, self.b, self.sw]


class ReservedGpio(BaseModel):
    """A GPIO line the board binds to a bus, a console or a peripheral, by the
    same numbering as ``buttons``. A button or encoder line may not sit on one."""

    gpio: int = Field(ge=0)
    header_pin: int | None = None
    use: str = ""


class BoardVariantMatch(BaseModel):
    """The host facts a board variant can be discriminated on.

    Only ``cpu_cores`` today, because that is what separates the two SoC bins
    a vendor can ship behind ONE device tree."""

    cpu_cores: int | None = None


class BoardVariant(BaseModel):
    """A same-device-tree hardware variant of a board.

    Radxa publishes a single device tree for the ROCK 5C (RK3588S2, 8 cores)
    and the ROCK 5C Lite (RK3582, 6 cores), so pattern matching cannot tell
    them apart and whichever profile filename sorted first won — a full 5C
    published ``soc: RK3582`` and a name ending in "Lite" all the way out to
    Mission Control. A variant names the discriminator explicitly and overrides
    only the identity fields that actually differ."""

    id: str
    # An EMPTY match never selects: a catch-all variant would silently rename
    # every unit of the board.
    when: BoardVariantMatch = Field(default_factory=BoardVariantMatch)
    name: str | None = None
    soc: str | None = None
    default_tier: int | None = None


class ImuBinding(BaseModel):
    """Where the board's primary IMU is read from: a host SPI/I2C chip, the
    USB sensor hub, or the flight controller over MAVLink."""

    bus: Literal["spi", "i2c", "hub", "fc"]
    model: str | None = None
    rate_hz: int | None = None


class GnssBinding(BaseModel):
    """Where the board's GNSS receiver bytes come from. ``pps_gpio`` is the GPIO
    number of the line carrying the receiver's PPS, when wired to the host: BCM
    numbering on a Raspberry Pi, the Linux GPIO number (``bank * 32 + line``,
    the vendor pinout's "GPIO number" column) on Rockchip."""

    transport: Literal["fc", "hub", "serial", "udp"]
    device: str | None = None
    baud: int | None = None
    pps_gpio: int | None = None
    model: str | None = None


class SensorHubBinding(BaseModel):
    """A USB sensor hub the board carries.

    Only the hub firmware's CDC id ``1209:ad05`` is a hub; the same boards on
    stock ArduPilot (``1209:5741``) are flight controllers, never a hub.
    """

    usb_vid_pid: str = Field(default="1209:ad05", pattern=r"^1209:(ad05|AD05)$")
    board: str | None = None


class BatteryGaugeBinding(BaseModel):
    """A host-I2C battery current/voltage gauge. ``shunt_ohm`` and
    ``max_current_a`` describe the rig's own shunt and are always required."""

    type: Literal["ina226"]
    i2c_bus: int
    addr: int
    shunt_ohm: float = Field(gt=0)
    max_current_a: float = Field(gt=0)
