"""Board profile models and the detected ``BoardInfo`` record.

Re-exported from ``ados.hal.detect``.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Literal

from pydantic import BaseModel, Field, model_validator


class DisplayGpio(BaseModel):
    """One GPIO pin claimed by a display binding.

    ``pin`` carries the SoC-native pin name (e.g. ``GPIO1_B0`` on Rockchip,
    ``PJ25`` on Allwinner). ``pinctrl`` selects which pin controller block
    drives the pad; Allwinner SoCs split a few pins into a separate AO
    controller addressed as ``r_pio`` while the bulk of the pins live on
    the default ``pio`` block. ``header_pin`` is the physical position on
    the 40-pin expansion header so a board's wiring can be traced from
    YAML alone without cross-referencing a pin-mux PDF.
    """

    pin: str
    pinctrl: str = "default"
    header_pin: int
    direction: Literal["out", "in"]


class DisplayTouch(BaseModel):
    """A standalone SPI resistive-touch controller attached to an HDMI display.

    Models an XPT2046 / ADS7846-compatible resistive touch controller that
    is wired to an SPI bus but has NO framebuffer panel of its own — the
    video comes over HDMI, only the touch overlay rides SPI. The installer
    compiles a touch-only device-tree overlay (an ``ads7846`` node with the
    pendown IRQ, no ILI9486 panel) and writes a ``LIBINPUT_CALIBRATION_MATRIX``
    udev rule so cage/libinput maps the resistive contact onto the HDMI
    output. It is the counterpart to the framebuffer fields on
    :class:`DisplayBinding`, which model an SPI-LCD panel instead.

    ``bus`` is the SPI controller id (``spi4`` on RK3588 maps to the
    ``spi@feb40000`` node). ``cs`` is the chip-select slot the touch chip
    sits on. ``irq`` is the pendown-interrupt GPIO — the ADS7846 PENIRQ line
    is open-drain active-low, so the overlay declares it ``GPIO_ACTIVE_LOW``.
    The device-tree overlay itself is named by the parent binding's
    ``overlay_ref``/``overlay_source``.

    ``x_min``/``x_max``/``y_min``/``y_max`` are the raw 12-bit ADC bounds the
    touch overlay actually reports at the panel edges. A resistive panel
    rarely spans the full 0..4095 range, so these bounds — together with the
    ``swap_xy``/``invert_x``/``invert_y`` orientation flags — are what the
    installer turns into the initial ``LIBINPUT_CALIBRATION_MATRIX``. They are
    a best-guess baseline; the on-screen calibration wizard refits them on
    the rig and regenerates the udev matrix.
    """

    controller: str = "XPT2046"
    bus: str
    cs: int = 0
    irq: DisplayGpio
    x_min: int = 0
    x_max: int = 4095
    y_min: int = 0
    y_max: int = 4095
    swap_xy: bool = False
    invert_x: bool = False
    invert_y: bool = False
    modules_required: list[str] = Field(default_factory=lambda: ["ads7846"])


class DisplayBinding(BaseModel):
    """One supported display and the wiring it claims on this board.

    Fields cover both the install-time provisioning (``overlay_source``,
    ``overlay_ref``, ``modules_required``) and the runtime contract that
    the on-board UI service needs to render to the right framebuffer
    (``resolution``, ``default_rotation``, ``gpio``).

    ``overlay_source = "repo"`` means the agent ships the DTS in
    ``data/overlays/<overlay_ref>`` and compiles + installs it during
    ``install-display-overlay.sh``. ``overlay_source = "upstream"`` means
    the BSP already provides a compiled DTBO and the installer activates
    it; the fallback path vendors a copy of the upstream source under
    ``data/overlays/upstream/`` when the BSP overlay set is absent.

    ``type = "hdmi-touch"`` describes an HDMI display carrying a standalone
    SPI resistive-touch controller. Video arrives over HDMI (the kernel DRM
    driver owns the framebuffer, so the ``framebuffer``/``controller`` panel
    fields do not apply), and the SPI touch overlay is modelled by the
    ``touch`` block. Every other ``type`` describes a panel whose own
    controller drives the framebuffer.

    A panel whose touch controller connects over USB instead of the GPIO/SPI
    header (many HDMI touchscreens expose a micro-USB "touch" port for exactly
    this) uses plain ``type = "hdmi"`` — the USB-HID touchscreen is auto-detected
    by libinput and reaches cage + Chromium with NO overlay, NO ``touch`` block,
    and no GPIO wiring. Only the SPI/GPIO ``ads7846`` case needs ``hdmi-touch``.
    """

    id: str
    type: Literal["spi-lcd", "hdmi", "hdmi-touch", "dpi", "mipi-dsi"]
    controller: str
    touch_chip: str | None = None
    bus: str
    resolution: str
    overlay_source: Literal["repo", "upstream", "upstream-vendored", "raspberrypi"] = "repo"
    overlay_ref: str
    gpio: dict[str, DisplayGpio] = Field(default_factory=dict)
    default_rotation: int = 0
    modules_required: list[str] = Field(default_factory=list)

    # Present only on ``type = "hdmi-touch"`` bindings: the standalone SPI
    # resistive-touch controller wired alongside the HDMI video output.
    touch: DisplayTouch | None = None

    @model_validator(mode="after")
    def _touch_required_for_hdmi_touch(self) -> DisplayBinding:
        """An ``hdmi-touch`` binding must carry a ``touch`` block, and only it.

        The ``touch`` block is the whole point of the ``hdmi-touch`` type, so
        a binding that declares the type without one is a config error the
        provisioner cannot act on. Conversely a non-``hdmi-touch`` binding
        that carries a ``touch`` block is ambiguous, so both are rejected up
        front rather than silently ignored.
        """
        if self.type == "hdmi-touch" and self.touch is None:
            raise ValueError("type 'hdmi-touch' requires a 'touch' block")
        if self.type != "hdmi-touch" and self.touch is not None:
            raise ValueError("'touch' block is only valid on type 'hdmi-touch'")
        return self


class DisplaysSection(BaseModel):
    """All displays a board can drive, plus future top-level knobs."""

    supported: list[DisplayBinding] = Field(default_factory=list)


class CameraMode(BaseModel):
    """One capture mode a camera binding exposes on a specific video node.

    ``isp_flags`` are vendor-ISP source properties (e.g. the Allwinner
    ``en-awisp``/``en-largemode`` v4l2 source props) spliced into the
    capture element when ``vendor_isp`` is set on the parent binding.
    """

    node: str | None = None
    width: int | None = None
    height: int | None = None
    fps: int | None = None
    format: str | None = None
    isp_flags: list[str] = Field(default_factory=list)


class CameraBinding(BaseModel):
    """One supported camera and how the agent brings it up.

    Generalises the display binding: ``overlay_required``/``overlay_source``/
    ``overlay_ref`` drive install-time provisioning, ``vendor_isp``/
    ``isp_flags``/``modes`` drive the runtime capture pipeline. ``overlay_ref``
    is a glob token matched under ``/boot/dtbo`` (the BSP basename carries a
    board-family prefix), never a hardcoded full filename. ``overlay_source =
    "bsp-disabled"`` means the BSP ships the DTBO as ``<name>.dtbo.disabled``
    and the provisioner enables it in place.
    """

    id: str
    type: Literal["csi", "usb", "ip"] = "usb"
    sensor: str | None = None
    bus: str | None = None
    orientation: str = "forward"
    overlay_required: bool = False
    overlay_source: Literal[
        "repo", "upstream", "upstream-vendored", "raspberrypi", "bsp-disabled"
    ] = "repo"
    overlay_ref: str | None = None
    vendor_isp: bool = False
    isp_flags: list[str] = Field(default_factory=list)
    modules_required: list[str] = Field(default_factory=list)
    modes: list[CameraMode] = Field(default_factory=list)
    default_mode: str | None = None


class CamerasSection(BaseModel):
    """All cameras a board can drive. Boards with none leave this empty."""

    supported: list[CameraBinding] = Field(default_factory=list)


class RadioBinding(BaseModel):
    """One radio the board carries that the provisioner installs a driver for.

    Declarative only; runtime adapter selection is unchanged. The onboard
    management Wi-Fi is intentionally never listed here.
    """

    id: str
    role: str | None = None
    chipset: str | None = None
    driver: str | None = None
    install: str = "dkms"
    modules_required: list[str] = Field(default_factory=list)
    mesh_capable: bool = False


class RadiosSection(BaseModel):
    supported: list[RadioBinding] = Field(default_factory=list)


class FcInterface(BaseModel):
    """A priority-ordered flight-controller link hint.

    The provisioner seeds ``serial_port``/``baud_rate`` from the highest
    priority interface; the runtime router still probes and falls back.
    """

    id: str
    type: Literal["uart", "usb-acm", "usb"] = "uart"
    path: str | None = None
    baud: int | None = None
    baud_candidates: list[int] = Field(default_factory=list)
    priority: int = 10


class FlightControllerSection(BaseModel):
    interfaces: list[FcInterface] = Field(default_factory=list)


class VideoSection(BaseModel):
    """Video/encoder knobs. Extra keys (e.g. nav_cameras) are ignored."""

    csi_ports: int | None = None
    csi_connector: str | None = None
    max_encode_resolution: str | None = None
    max_encode_fps: int | None = None
    encoder_api: str | None = None


class ComputeSection(BaseModel):
    """Compute knobs. The NPU capability and the local-inference declaration are
    read here; extra keys (cores, gpu, hw_encoder, ram, ...) are ignored."""

    npu_tops: float = 0.0
    # Whether this board can run the detector locally WITHOUT an NPU, on the CPU
    # via the in-process ONNX backend. "none" (default) = no CPU-inference path;
    # "onnx" = a CPU strong enough for the ONNX detector (declared only on boards
    # where it is genuinely usable). Drives the perception tier
    # (a capable board reads `local`) and the installer's vision-binary variant
    # selection (a capable board fetches the onnx-enabled ados-vision build).
    local_inference: Literal["none", "onnx"] = "none"


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
    """The panel button roles a board wires, by BCM pin. Optional: a board
    without the ``buttons:`` block keeps the panel's built-in pin fallback."""

    up: int
    down: int
    select: int
    back: int


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
    """Where the board's GNSS receiver bytes come from. ``pps_gpio`` is the BCM
    line carrying the receiver's PPS, when wired to the host."""

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
    """A host-I2C battery current/voltage gauge."""

    type: Literal["ina226"]
    i2c_bus: int
    addr: int
    shunt_ohm: float
    max_current_a: float


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
    flight_controller: FlightControllerSection = Field(
        default_factory=FlightControllerSection
    )
    variants: list[BoardVariant] = Field(default_factory=list)

    # Optional sensor bindings for capture rigs and gauge-equipped boards.
    # Absent on boards that carry none of them.
    imu: ImuBinding | None = None
    gnss: GnssBinding | None = None
    sensor_hub: SensorHubBinding | None = None
    battery_gauge: BatteryGaugeBinding | None = None

    # The profile's YAML filename stem (``cubie-a7s``, ``rock-5c-lite``). Not a
    # YAML key: ``_load_board_profiles`` stamps it from the file name, and it is
    # the ONE canonical token ``/etc/ados/board_override`` accepts.
    stem: str = ""


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
            "has_local_inference": self.has_local_inference,
        }
