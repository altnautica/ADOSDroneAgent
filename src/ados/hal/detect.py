"""Hardware Abstraction Layer for board detection and profiling."""

from __future__ import annotations

import platform
import threading
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal

import yaml
from pydantic import BaseModel, Field, model_validator

from ados.core.logging import get_logger
from ados.core.paths import BOARD_OVERRIDE_PATH as _BOARD_OVERRIDE_PATH

log = get_logger("hal")

BOARDS_DIR = Path(__file__).parent / "boards"
BOARD_OVERRIDE_PATH = _BOARD_OVERRIDE_PATH


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
            "has_local_inference": self.has_local_inference,
        }


def detect_tier(ram_mb: int) -> int:
    """Assign tier based on available RAM.

    Tier 1: <512 MB (no compute)
    Tier 2: 512-2048 MB (basic compute)
    Tier 3: 2048-4096 MB (full ADOS)
    Tier 4: >4096 MB (swarm capable)
    """
    if ram_mb < 512:
        return 1
    if ram_mb < 2048:
        return 2
    if ram_mb <= 4096:
        return 3
    return 4


def _load_board_profiles() -> list[BoardProfile]:
    """Load all YAML board profiles, validated via Pydantic."""
    profiles: list[BoardProfile] = []
    if BOARDS_DIR.is_dir():
        for yaml_file in sorted(BOARDS_DIR.glob("*.yaml")):
            with open(yaml_file) as f:
                data = yaml.safe_load(f)
                if data:
                    profile = BoardProfile(**data)
                    # The filename stem is the canonical board-override token;
                    # it is not a YAML key, so it is stamped here.
                    profile.stem = yaml_file.stem
                    profiles.append(profile)
    return profiles


def known_board_stems() -> list[str]:
    """Every legal ``/etc/ados/board_override`` value, sorted.

    The override grammar is the board-profile YAML filename stem
    (``cubie-a7s``, ``rock-5c-lite``) and nothing else. It used to be matched
    against ``profile.name``, i.e. the display name — and the setup facade
    rejects anything outside ``[A-Za-z0-9_-]``, so 20 of the 21 display names
    could not be entered at all and the documented escape hatch for a
    mis-detected board was unusable. The stem is a legal slug, unambiguous, and
    what the shell scripts and the Rust sidecar writer can pass.
    """
    return sorted(p.stem for p in _load_board_profiles() if p.stem)


def _read_device_model() -> str:
    """Read board model from /proc/device-tree/model."""
    try:
        model_path = Path("/proc/device-tree/model")
        if model_path.exists():
            return model_path.read_text().strip().rstrip("\x00")
    except OSError:
        pass
    return ""


def _read_device_compatible() -> str:
    """Return the most-specific board token from /proc/device-tree/compatible.

    The compatible node is a NUL-separated, most-specific-first list
    (e.g. ``radxa,cubie-a7s\x00arm,sun60iw2p1\x00allwinner,sun60i-a733``). Its
    first token uniquely identifies the board, which disambiguates boards that
    share a generic device-tree ``model`` string -- every Allwinner A733 board
    reports ``sun60iw2`` as the model, so only the compatible node tells the
    Cubie A7S apart from the A7Z. Only the first token is returned so the
    generic SoC-family tokens later in the list never enter pattern matching.
    Returns "" when unavailable.
    """
    try:
        compat_path = Path("/proc/device-tree/compatible")
        if compat_path.exists():
            for token in compat_path.read_bytes().split(b"\x00"):
                token = token.strip()
                if token:
                    return token.decode("utf-8", "ignore")
    except OSError:
        pass
    return ""


def _read_cpuinfo_model() -> str:
    """Fallback: read board model from /proc/cpuinfo Hardware or model lines."""
    try:
        cpuinfo_path = Path("/proc/cpuinfo")
        if cpuinfo_path.exists():
            text = cpuinfo_path.read_text()
            for line in text.splitlines():
                lower = line.lower()
                if lower.startswith("hardware") or lower.startswith("model"):
                    parts = line.split(":", 1)
                    if len(parts) == 2:
                        value = parts[1].strip()
                        if value:
                            return value
    except OSError:
        pass
    return ""


def _read_board_override() -> str:
    """The operator's forced board — a board-profile YAML filename STEM.

    ``/etc/ados/board_override`` has five consumers across two languages; the
    stem is the one grammar all of them accept (see ``known_board_stems``).
    """
    try:
        if BOARD_OVERRIDE_PATH.exists():
            content = BOARD_OVERRIDE_PATH.read_text().strip()
            if content:
                return content
    except OSError:
        pass
    return ""


def _match_profile(
    profiles: list[BoardProfile], model_string: str
) -> BoardProfile | None:
    """Find the first profile whose pattern matches the model string."""
    model_lower = model_string.lower()
    for profile in profiles:
        for pattern in profile.model_patterns:
            if pattern.lower() in model_lower:
                return profile
    return None


def _match_override(
    profiles: list[BoardProfile], token: str
) -> BoardProfile | None:
    """Resolve a board-override token (a YAML filename stem) to its profile."""
    wanted = token.strip().lower()
    if not wanted:
        return None
    for profile in profiles:
        if profile.stem.lower() == wanted:
            return profile
    return None


def _select_variant(profile: BoardProfile, cpu_cores: int) -> BoardVariant | None:
    """The variant whose ``when`` holds for these facts, if any.

    An empty ``when`` never selects — a catch-all variant would rename every
    unit of the board silently.
    """
    for variant in profile.variants:
        if variant.when.cpu_cores is not None and variant.when.cpu_cores == cpu_cores:
            return variant
    return None


def _apply_variant(profile: BoardProfile, cpu_cores: int) -> BoardProfile:
    """Return `profile` with its matching same-device-tree variant applied.

    Returned as a copy so the loaded profile list stays the file's own content
    and a second resolution with different facts cannot see the first's
    overrides.
    """
    variant = _select_variant(profile, cpu_cores)
    if variant is None:
        return profile
    updates: dict[str, object] = {}
    if variant.name is not None:
        updates["name"] = variant.name
    if variant.soc is not None:
        updates["soc"] = variant.soc
    if variant.default_tier is not None:
        updates["default_tier"] = variant.default_tier
    if not updates:
        return profile
    return profile.model_copy(update=updates)


def _board_from_profile(
    profile: BoardProfile,
    model_string: str,
    ram_mb: int,
    cpu_cores: int,
) -> BoardInfo:
    """Build a BoardInfo from a matched profile, variant applied."""
    profile = _apply_variant(profile, cpu_cores)
    tier = profile.default_tier
    return BoardInfo(
        name=profile.name,
        model=model_string or profile.name,
        tier=tier,
        ram_mb=ram_mb,
        cpu_cores=cpu_cores,
        vendor=profile.vendor,
        soc=profile.soc,
        arch=profile.arch,
        hw_video_codecs=list(profile.hw_video_codecs),
        npu_tops=profile.compute.npu_tops,
        local_inference=profile.compute.local_inference,
    )


# Board hardware does not change at runtime, so a single cached value is reused
# for the service lifetime. invalidate_board_info_cache() clears it for tests
# or in case the override file is changed by an operator action.
_BOARD_INFO_CACHE: BoardInfo | None = None
_BOARD_PROFILE_CACHE: BoardProfile | None = None
_BOARD_INFO_CACHE_LOCK = threading.Lock()


def invalidate_board_info_cache() -> None:
    """Clear the cached board detection result."""
    global _BOARD_INFO_CACHE, _BOARD_PROFILE_CACHE
    with _BOARD_INFO_CACHE_LOCK:
        _BOARD_INFO_CACHE = None
        _BOARD_PROFILE_CACHE = None


def detect_board(force: bool = False) -> BoardInfo:
    """Detect the current board.

    Detection order:
    1. /etc/ados/board_override, resolved by board-profile YAML filename stem
    2. /proc/device-tree/compatible first token (uniquely identifies a board)
    3. /proc/device-tree/model, matched against board profile patterns
    4. /proc/cpuinfo Hardware/model, fallback pattern matching
    5. The generic profile for this architecture (never a profile-less record)

    Result is cached for the service lifetime. Pass force=True to bypass
    the cache and re-run detection.
    """
    global _BOARD_INFO_CACHE
    if not force and _BOARD_INFO_CACHE is not None:
        return _BOARD_INFO_CACHE
    with _BOARD_INFO_CACHE_LOCK:
        if not force and _BOARD_INFO_CACHE is not None:
            return _BOARD_INFO_CACHE
        info = _detect_board_uncached()
        _BOARD_INFO_CACHE = info
        return info


def _resolve_profile_match(
    profiles: list[BoardProfile],
) -> tuple[BoardProfile, str, str] | None:
    """Resolve the board profile for this host.

    Returns ``(profile, model_string, source)`` or ``None`` when nothing
    matches. Mirrors the Rust ``board_sidecar::resolve_profile`` exactly — the
    two halves must name the same board for the same hardware, because one
    writes ``/run/ados/board.json`` and the other answers the Python services.

    An override naming no profile falls THROUGH to auto-detection with a
    warning rather than minting a profile-less board: the override exists to
    correct a mis-detection, and a typo must not cost the node its UART
    candidates and its perception tier.
    """
    model_string = _read_device_model()
    compat_string = _read_device_compatible()
    cpuinfo_model = _read_cpuinfo_model()

    token = _read_board_override()
    if token:
        matched = _match_override(profiles, token)
        if matched:
            detected = model_string or compat_string or cpuinfo_model
            return matched, detected or token, "override"
        log.warning(
            "board_override_unknown",
            token=token,
            hint="expected a board-profile YAML filename stem, e.g. rock-5c-lite",
        )

    if compat_string:
        matched = _match_profile(profiles, compat_string)
        if matched:
            return matched, model_string or compat_string, "compatible"
    if model_string:
        matched = _match_profile(profiles, model_string)
        if matched:
            return matched, model_string, "model"
    if cpuinfo_model:
        matched = _match_profile(profiles, cpuinfo_model)
        if matched:
            return matched, cpuinfo_model, "cpuinfo"
    return None


def _generic_identity() -> tuple[str, str]:
    """The generic profile stem for this architecture, and the name an
    unmatched board is published under.

    Kept together because the Rust sidecar writer resolves the identical pair.
    The two used to disagree: Rust resolved an unmatched board through the
    ``generic-arm64`` profile (keeping its fallback UART candidates), while this
    half minted a bare ``generic-aarch64`` record that matches no profile at all
    — so the half that answers the FC-link probe had no candidates to offer on
    exactly the hardware that needs a fallback most.
    """
    machine = platform.machine()
    stem = "generic-x86_64" if machine in ("x86_64", "AMD64") else "generic-arm64"
    # A dev Mac is not a board; say so rather than claiming an SBC profile.
    name = "macOS (dev)" if platform.system() == "Darwin" else stem
    return stem, name


def _match_current_profile() -> BoardProfile | None:
    """Run the override/device-tree/cpuinfo match and return the BoardProfile.

    Returns the full validated profile (with the declarative
    cameras/radios/video/FC blocks) instead of the flat ``BoardInfo``, with any
    same-device-tree variant applied. An unmatched board resolves through the
    generic profile for its architecture — the same degradation the Rust half
    performs — so a caller still gets the fallback UART candidates. ``None``
    only when even that profile is missing.
    """
    import psutil

    profiles = _load_board_profiles()
    cpu_cores = psutil.cpu_count(logical=True) or 1
    matched = _resolve_profile_match(profiles)
    if matched is not None:
        return _apply_variant(matched[0], cpu_cores)
    generic_stem, _name = _generic_identity()
    for profile in profiles:
        if profile.stem == generic_stem:
            return profile
    return None


def detect_board_profile(force: bool = False) -> BoardProfile | None:
    """Return the matched full board profile for the current hardware.

    Services that need the declarative blocks (``cameras``, ``radios``,
    ``video.encoder_api``, ``flight_controller``) call this; everything else
    uses the lighter ``detect_board``. Cached for the service lifetime.
    An unmatched board resolves through the generic profile for its
    architecture, so the fallback UART candidates exist; ``None`` only when
    even that profile is missing from the directory.
    """
    global _BOARD_PROFILE_CACHE
    if not force and _BOARD_PROFILE_CACHE is not None:
        return _BOARD_PROFILE_CACHE
    with _BOARD_INFO_CACHE_LOCK:
        if not force and _BOARD_PROFILE_CACHE is not None:
            return _BOARD_PROFILE_CACHE
        _BOARD_PROFILE_CACHE = _match_current_profile()
        return _BOARD_PROFILE_CACHE


def _detect_board_uncached() -> BoardInfo:
    """Run the full detection pipeline without consulting the cache."""
    import psutil

    ram_mb = psutil.virtual_memory().total // (1024 * 1024)
    cpu_cores = psutil.cpu_count(logical=True) or 1
    profiles = _load_board_profiles()

    matched = _resolve_profile_match(profiles)
    if matched is not None:
        profile, model_string, source = matched
        board = _board_from_profile(profile, model_string, ram_mb, cpu_cores)
        log.info(
            "board_detected",
            board=board.name,
            source=source,
            tier=board.tier,
            ram_mb=ram_mb,
        )
        return board

    # Unmatched. Resolve through the generic profile for this architecture
    # rather than minting a bare record: that profile exists to carry the
    # fallback UART candidates and camera defaults, and a board with no profile
    # at all is worse off on exactly the hardware that needs a fallback most.
    # The Rust sidecar writer degrades identically.
    generic_stem, fallback_name = _generic_identity()
    detected_model = (
        _read_device_model() or _read_cpuinfo_model() or _read_device_compatible()
    )
    for profile in profiles:
        if profile.stem == generic_stem:
            board = _board_from_profile(
                profile, detected_model or fallback_name, ram_mb, cpu_cores
            )
            board.name = fallback_name
            # The generic profile's default_tier is a placeholder; an unknown
            # board is tiered from what it actually has.
            board.tier = detect_tier(ram_mb)
            log.info("board_fallback", board=board.name, tier=board.tier, ram_mb=ram_mb)
            return board

    board = BoardInfo(
        name=fallback_name,
        model=detected_model or fallback_name,
        tier=detect_tier(ram_mb),
        ram_mb=ram_mb,
        cpu_cores=cpu_cores,
    )
    log.info("board_fallback", board=board.name, tier=board.tier, ram_mb=ram_mb)
    return board
