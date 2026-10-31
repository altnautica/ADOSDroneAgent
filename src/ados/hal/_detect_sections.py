"""Board profile section models: displays, cameras, radios, FC, video, compute."""

from __future__ import annotations

from typing import Literal

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
