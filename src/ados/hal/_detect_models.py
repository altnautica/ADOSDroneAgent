"""Board profile models and the detected ``BoardInfo`` record.

Re-exported from ``ados.hal.detect``.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from pydantic import BaseModel, Field, model_validator

from ados.hal._detect_bindings import (
    BatteryGaugeBinding,
    BoardVariant,
    ButtonPins,
    EncoderPins,
    GnssBinding,
    GpioButton,
    GpioOutput,
    ImuBinding,
    ReservedGpio,
    SensorHubBinding,
)
from ados.hal._detect_sections import (
    CamerasSection,
    ComputeSection,
    DisplaysSection,
    FlightControllerSection,
    RadiosSection,
    VideoSection,
)


class BoardProfile(BaseModel):
    """Pydantic model for YAML board profile validation."""

    name: str
    vendor: str = "unknown"
    soc: str = "unknown"
    arch: str = "aarch64"
    model_patterns: list[str] = []
    default_tier: int = 2
    gpio_pins: list[int] = []
    gpio_outputs: list[GpioOutput] = Field(default_factory=list)
    gpio_buttons: list[GpioButton] = Field(default_factory=list)
    buttons: ButtonPins | None = None
    encoder: EncoderPins | None = None
    uart_paths: list[str] = []
    hw_video_codecs: list[str] = []

    # Optional field. Defaults to None so existing YAMLs without it load
    # unchanged.
    min_kernel_version: str | None = None

    # Optional displays section consumed by the LCD-overlay installer and
    # by the renderer-adapter UI service. Boards without any local display
    # leave this absent and resolve to an empty supported list.
    displays: DisplaysSection = Field(default_factory=DisplaysSection)

    # Optional declarative hardware sections consumed by the universal
    # provisioner (install-time) and the camera/radio/FC services (runtime).
    # All default empty so boards that omit them load unchanged.
    video: VideoSection = Field(default_factory=VideoSection)
    compute: ComputeSection = Field(default_factory=ComputeSection)
    cameras: CamerasSection = Field(default_factory=CamerasSection)
    radios: RadiosSection = Field(default_factory=RadiosSection)
    flight_controller: FlightControllerSection = Field(default_factory=FlightControllerSection)
    variants: list[BoardVariant] = Field(default_factory=list)

    # Optional sensor bindings for capture rigs and gauge-equipped boards.
    # Absent on boards that carry none of them.
    imu: ImuBinding | None = None
    gnss: GnssBinding | None = None
    sensor_hub: SensorHubBinding | None = None
    battery_gauge: BatteryGaugeBinding | None = None

    # GPIO lines bound to something other than a button (buses, the debug
    # console, panel control lines). The GNSS PPS line counts as reserved too.
    reserved_gpio: list[ReservedGpio] = Field(default_factory=list)

    # The profile's YAML filename stem (``cubie-a7s``, ``rock-5c-lite``). Not a
    # YAML key: ``_load_board_profiles`` stamps it from the file name, and it is
    # the ONE canonical token ``/etc/ados/board_override`` accepts.
    stem: str = ""

    def reserved_lines(self) -> list[int]:
        """Every GPIO number bound to something other than a button."""
        lines = {r.gpio for r in self.reserved_gpio}
        if self.gnss is not None and self.gnss.pps_gpio is not None:
            lines.add(self.gnss.pps_gpio)
        return sorted(lines)

    @model_validator(mode="after")
    def _panel_inputs_clear_of_reserved_lines(self) -> BoardProfile:
        declared: list[tuple[str, int]] = []
        if self.buttons is not None:
            declared += [("button", p) for p in self.buttons.pins()]
        if self.encoder is not None:
            declared += [("encoder", p) for p in self.encoder.pins()]
        reserved = set(self.reserved_lines())
        seen: set[int] = set()
        for role, pin in declared:
            if pin in seen:
                raise ValueError(f"{role} gpio {pin} is declared twice")
            if pin in reserved:
                raise ValueError(f"{role} gpio {pin} is a reserved line")
            seen.add(pin)
        return self


@dataclass
class BoardInfo:
    name: str
    model: str
    tier: int
    ram_mb: int
    cpu_cores: int
    vendor: str = "unknown"
    soc: str = "unknown"
    arch: str = "aarch64"
    hw_video_codecs: list[str] = field(default_factory=list)
    # NPU throughput in TOPS from the board profile's compute section (0.0 when
    # the board has no NPU / an unknown board). The perception tier keys on this:
    # an accelerator runs detection locally; a board without one offloads.
    npu_tops: float = 0.0
    # The board profile's local-inference declaration ("none" | "onnx"). "onnx"
    # marks an NPU-less but CPU-strong board that runs the detector on-board via
    # the in-process ONNX backend. Empty string / "none" ⇒ no CPU-inference path.
    local_inference: str = "none"
    # The board-profile YAML filename stem (``cm4``, ``rock-5c-lite``): the
    # canonical token plugin ``supported_boards`` match against. Empty when no
    # profile resolved.
    profile: str = ""
    # Panel button pins from the profile's optional ``buttons:`` block.
    buttons: ButtonPins | None = None
    # Rotary-encoder pins from the profile's optional ``encoder:`` block.
    encoder: EncoderPins | None = None

    @property
    def has_accelerator(self) -> bool:
        """Whether the board has a usable inference accelerator (an NPU)."""
        return self.npu_tops > 0.0

    @property
    def has_local_inference(self) -> bool:
        """Whether the board can run the detector locally on the CPU (no NPU),
        via the in-process ONNX backend. A full local perception path: the
        perception tier reads `local` for such a board the same way an NPU board
        does."""
        return self.local_inference not in ("", "none")

    def to_dict(self) -> dict[str, Any]:
        """The board fingerprint document's field set.

        This is the Python side of the ``/run/ados/board.json`` sidecar
        contract. The sidecar itself is WRITTEN by the supervisor, in Rust
        (``crates/ados-hal-probe/src/board_sidecar.rs``), because a lean
        zero-Python node has to report its real board and NPU too — and a second
        writer of one document is how it ended up with no writer at all on that
        profile. The Rust document is this key set plus the ``version`` the
        sidecar registry declares; a parity test holds the two together.
        """
        return {
            "name": self.name,
            "profile": self.profile,
            "model": self.model,
            "tier": self.tier,
            "ram_mb": self.ram_mb,
            "cpu_cores": self.cpu_cores,
            "vendor": self.vendor,
            "soc": self.soc,
            "arch": self.arch,
            "hw_video_codecs": self.hw_video_codecs,
            "npu_tops": self.npu_tops,
            "has_accelerator": self.has_accelerator,
            "local_inference": self.local_inference,
            "buttons": self.buttons.model_dump() if self.buttons else None,
            "encoder": self.encoder.model_dump() if self.encoder else None,
            "has_local_inference": self.has_local_inference,
        }
