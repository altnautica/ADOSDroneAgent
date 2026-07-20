"""Hardware Abstraction Layer for board detection and profiling."""

from __future__ import annotations

import platform
import threading
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal

import yaml
from pydantic import BaseModel, Field

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
    """

    id: str
    type: Literal["spi-lcd", "hdmi", "dpi", "mipi-dsi"]
    controller: str
    touch_chip: str | None = None
    bus: str
    resolution: str
    overlay_source: Literal["repo", "upstream", "upstream-vendored", "raspberrypi"] = "repo"
    overlay_ref: str
    gpio: dict[str, DisplayGpio] = Field(default_factory=dict)
    default_rotation: int = 0
    modules_required: list[str] = Field(default_factory=list)


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
    # where it is genuinely usable, per Rule 44). Drives the perception tier
    # (a capable board reads `local`) and the installer's vision-binary variant
    # selection (a capable board fetches the onnx-enabled ados-vision build).
    local_inference: Literal["none", "onnx"] = "none"


class BoardProfile(BaseModel):
    """Pydantic model for YAML board profile validation."""

    name: str
    vendor: str = "unknown"
    soc: str = "unknown"
    arch: str = "aarch64"
    model_patterns: list[str] = []
    default_tier: int = 2
    gpio_pins: list[int] = []
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


def persist_board_sidecar(board: BoardInfo) -> bool:
    """Write the detected board fingerprint to the board sidecar file.

    The native control surface reads this sidecar for the board's NPU capability
    and the perception tier (``npu_tops`` -> ``has_accelerator`` -> local vs
    offload). Nothing else writes it, so a board that never persists it reports
    ``npu_tops: 0`` regardless of a real accelerator. This is called once at
    startup, right after detection, so the reader sees the true capability.

    Written atomically (tmp + replace). Best-effort: a run dir that is not yet
    writable is not fatal (the value self-heals on the next call), so a failure
    returns ``False`` rather than blocking startup. Returns ``True`` on success.
    """
    import json

    from ados.core.paths import BOARD_JSON

    try:
        BOARD_JSON.parent.mkdir(parents=True, exist_ok=True)
        tmp = BOARD_JSON.with_name(BOARD_JSON.name + ".tmp")
        tmp.write_text(json.dumps(board.to_dict()))
        tmp.replace(BOARD_JSON)
        return True
    except OSError:
        return False


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
                    profiles.append(BoardProfile(**data))
    return profiles


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
    """Check /etc/ados/board_override for a forced board name."""
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


def _board_from_profile(
    profile: BoardProfile,
    model_string: str,
    ram_mb: int,
    cpu_cores: int,
) -> BoardInfo:
    """Build a BoardInfo from a matched profile."""
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
    1. /etc/ados/board_override, if present, load that profile by name
    2. /proc/device-tree/model, match against board profile patterns
    3. /proc/cpuinfo Hardware/model, fallback pattern matching
    4. Platform-based fallback (macOS dev, generic-x86_64, generic-<arch>)

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


def _match_current_profile() -> BoardProfile | None:
    """Run the override/device-tree/cpuinfo match and return the BoardProfile.

    Mirrors the matching order of ``_detect_board_uncached`` but returns the
    full validated profile (with the declarative cameras/radios/video/FC
    blocks) instead of the flat ``BoardInfo``. Returns None when nothing
    matches (generic fallback).
    """
    profiles = _load_board_profiles()
    override_name = _read_board_override()
    if override_name:
        for profile in profiles:
            if profile.name.lower() == override_name.lower():
                return profile
    compat_string = _read_device_compatible()
    if compat_string:
        matched = _match_profile(profiles, compat_string)
        if matched:
            return matched
    model_string = _read_device_model()
    if model_string:
        matched = _match_profile(profiles, model_string)
        if matched:
            return matched
    cpuinfo_model = _read_cpuinfo_model()
    if cpuinfo_model:
        matched = _match_profile(profiles, cpuinfo_model)
        if matched:
            return matched
    return None


def detect_board_profile(force: bool = False) -> BoardProfile | None:
    """Return the matched full board profile for the current hardware.

    Services that need the declarative blocks (``cameras``, ``radios``,
    ``video.encoder_api``, ``flight_controller``) call this; everything else
    uses the lighter ``detect_board``. Cached for the service lifetime.
    Returns None on the generic fallback (no matching profile).
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

    # 1. Board override file
    override_name = _read_board_override()
    if override_name:
        for profile in profiles:
            if profile.name.lower() == override_name.lower():
                board = _board_from_profile(profile, override_name, ram_mb, cpu_cores)
                log.info("board_override", board=board.name, tier=board.tier)
                return board
        # Override name did not match any profile, use it as a raw name
        tier = detect_tier(ram_mb)
        board = BoardInfo(
            name=override_name,
            model=override_name,
            tier=tier,
            ram_mb=ram_mb,
            cpu_cores=cpu_cores,
        )
        log.info("board_override_unmatched", board=board.name, tier=board.tier)
        return board

    # 2. Device-tree detection. Match the most-specific compatible token first
    #    (it uniquely identifies the board), then the model string (which can be
    #    a generic SoC-family name shared by several boards -- every Allwinner
    #    A733 board reports "sun60iw2" as the model, so only the compatible node
    #    tells the Cubie A7S apart from the A7Z).
    model_string = _read_device_model()
    compat_string = _read_device_compatible()
    if compat_string:
        matched = _match_profile(profiles, compat_string)
        if matched:
            board = _board_from_profile(
                matched, model_string or compat_string, ram_mb, cpu_cores
            )
            log.info(
                "board_detected_compatible",
                board=board.name,
                tier=board.tier,
                ram_mb=ram_mb,
            )
            return board
    if model_string:
        matched = _match_profile(profiles, model_string)
        if matched:
            board = _board_from_profile(matched, model_string, ram_mb, cpu_cores)
            log.info("board_detected", board=board.name, tier=board.tier, ram_mb=ram_mb)
            return board

    # 3. /proc/cpuinfo fallback
    cpuinfo_model = _read_cpuinfo_model()
    if cpuinfo_model:
        matched = _match_profile(profiles, cpuinfo_model)
        if matched:
            board = _board_from_profile(matched, cpuinfo_model, ram_mb, cpu_cores)
            log.info(
                "board_detected_cpuinfo",
                board=board.name,
                tier=board.tier,
                ram_mb=ram_mb,
            )
            return board

    # 4. Platform fallback
    tier = detect_tier(ram_mb)
    system = platform.system()
    machine = platform.machine()
    if system == "Darwin":
        fallback_name = "macOS (dev)"
    elif machine in ("x86_64", "AMD64"):
        fallback_name = "generic-x86_64"
    else:
        fallback_name = f"generic-{machine}"
    board = BoardInfo(
        name=fallback_name,
        model=model_string or cpuinfo_model or f"{system} {machine}",
        tier=tier,
        ram_mb=ram_mb,
        cpu_cores=cpu_cores,
    )
    log.info("board_fallback", board=board.name, tier=board.tier, ram_mb=ram_mb)
    return board
