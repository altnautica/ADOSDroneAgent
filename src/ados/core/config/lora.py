"""LoRa beacon and mesh lane (the ``lora:`` block).

The native ``ados-lora`` service reads this block. Transmit is governed by a
compiled regional band plan: the region is ``region`` when set, else the
agent's country setting ``network.regulatory.region``; with neither, transmit
is blocked and receive stays on.
"""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field, model_validator


class LoraE22Config(BaseModel):
    """The Ebyte E22 UART module."""

    port: str = "/dev/ttyUSB0"
    baud: int = Field(default=9600, ge=1200, le=115200)
    gpio_chip: str = "gpiochip0"
    # M0/M1 mode line offsets; None when the lines are strapped.
    m0_gpio: int | None = Field(default=None, ge=0, le=1023)
    m1_gpio: int | None = Field(default=None, ge=0, le=1023)
    # The fitted module's class: "850-930" (E22-900) or "410-493" (E22-400).
    module_band: Literal["850-930", "410-493"] = "850-930"


class LoraMeshtasticConfig(BaseModel):
    """The Meshtastic node bridge."""

    enabled: bool = False
    port: str | None = None


class LoraLoopbackConfig(BaseModel):
    """The UDP stand-in radio for simulation."""

    bind: str = "0.0.0.0:7810"
    peer: str = "127.0.0.1:7811"
    module_band: Literal["any", "850-930", "410-493", "863-928"] = "any"


class LoraConfig(BaseModel):
    enabled: bool = False
    transport: Literal["e22", "meshtastic", "loopback"] = "e22"
    # Region code (IN, EU, EU433, AU433 or an ISO 3166-1 alpha-2 country);
    # None follows network.regulatory.region.
    region: str | None = None
    node_id: int = Field(default=1, ge=0, le=65535)
    # Fixed channel centre; None uses the plan default (or hops where the
    # plan hops).
    freq_mhz: float | None = Field(default=None, ge=0, le=6000)
    # Requested ERP; None runs at the plan clamp.
    erp_mw: float | None = Field(default=None, gt=0, le=4000)
    sf: int = Field(default=9, ge=7, le=12)
    bw_khz: int = Field(default=125, ge=7, le=500)
    # Coding rate index 1..4 (4/5..4/8).
    cr: int = Field(default=1, ge=1, le=4)
    preamble: int = Field(default=8, ge=6, le=65535)
    air_rate: Literal["2.4k"] = "2.4k"
    antenna_gain_dbi: float = Field(default=2.15, ge=-10, le=20)
    cable_loss_db: float = Field(default=0.0, ge=0, le=30)
    beacon_interval_s: int = Field(default=30, ge=1, le=3600)
    ledger_path: str = "/var/ados/lora/airtime.jsonl"
    e22: LoraE22Config = LoraE22Config()
    meshtastic: LoraMeshtasticConfig = LoraMeshtasticConfig()
    loopback: LoraLoopbackConfig = LoraLoopbackConfig()

    @model_validator(mode="after")
    def _strapped_e22_needs_a_channel(self) -> LoraConfig:
        # A strapped E22 (mode lines not wired) cannot enter configuration
        # mode, so it cannot be retuned; the lane must be told its channel.
        strapped = self.e22.m0_gpio is None or self.e22.m1_gpio is None
        if self.enabled and self.transport == "e22" and strapped and self.freq_mhz is None:
            raise ValueError(
                "lora.e22 mode lines are strapped (m0_gpio / m1_gpio unset): set "
                "lora.freq_mhz to the channel the module is configured for, or wire "
                "the mode lines"
            )
        return self
