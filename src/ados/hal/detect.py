"""Hardware Abstraction Layer for board detection and profiling."""

from __future__ import annotations

import platform
import threading
from pathlib import Path

import yaml

from ados.core.logging import get_logger
from ados.core.paths import BOARD_OVERRIDE_PATH as _BOARD_OVERRIDE_PATH
from ados.hal._detect_bindings import (
    BatteryGaugeBinding,
    BoardVariant,
    BoardVariantMatch,
    ButtonPins,
    EncoderPins,
    GnssBinding,
    GpioButton,
    GpioOutput,
    ImuBinding,
    SensorHubBinding,
)
from ados.hal._detect_models import (
    BoardInfo,
    BoardProfile,
)
from ados.hal._detect_sections import (
    CameraBinding,
    CameraMode,
    CamerasSection,
    ComputeSection,
    DisplayBinding,
    DisplayGpio,
    DisplaysSection,
    DisplayTouch,
    FcInterface,
    FlightControllerSection,
    RadioBinding,
    RadiosSection,
    VideoSection,
)

__all__ = [
    "BOARDS_DIR",
    "BOARD_OVERRIDE_PATH",
    "BatteryGaugeBinding",
    "BoardInfo",
    "BoardProfile",
    "BoardVariant",
    "BoardVariantMatch",
    "ButtonPins",
    "CameraBinding",
    "CameraMode",
    "CamerasSection",
    "ComputeSection",
    "DisplayBinding",
    "DisplayGpio",
    "DisplayTouch",
    "DisplaysSection",
    "EncoderPins",
    "FcInterface",
    "FlightControllerSection",
    "GnssBinding",
    "GpioButton",
    "GpioOutput",
    "ImuBinding",
    "RadioBinding",
    "RadiosSection",
    "SensorHubBinding",
    "VideoSection",
    "detect_board",
    "detect_board_profile",
    "detect_tier",
    "invalidate_board_info_cache",
    "known_board_stems",
]

log = get_logger("hal")

BOARDS_DIR = Path(__file__).parent / "boards"
BOARD_OVERRIDE_PATH = _BOARD_OVERRIDE_PATH


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


def _match_profile(profiles: list[BoardProfile], model_string: str) -> BoardProfile | None:
    """Find the first profile whose pattern matches the model string."""
    model_lower = model_string.lower()
    for profile in profiles:
        for pattern in profile.model_patterns:
            if pattern.lower() in model_lower:
                return profile
    return None


def _match_override(profiles: list[BoardProfile], token: str) -> BoardProfile | None:
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
        profile=profile.stem,
        buttons=profile.buttons,
        encoder=profile.encoder,
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
    detected_model = _read_device_model() or _read_cpuinfo_model() or _read_device_compatible()
    for profile in profiles:
        if profile.stem == generic_stem:
            board = _board_from_profile(profile, detected_model or fallback_name, ram_mb, cpu_cores)
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
