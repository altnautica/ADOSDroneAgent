"""Hardware Abstraction Layer for board detection and profiling."""

from __future__ import annotations

import platform
import threading
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml
from pydantic import BaseModel

from ados.core.logging import get_logger
from ados.core.paths import BOARD_OVERRIDE_PATH as _BOARD_OVERRIDE_PATH

log = get_logger("hal")

BOARDS_DIR = Path(__file__).parent / "boards"
BOARD_OVERRIDE_PATH = _BOARD_OVERRIDE_PATH


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
    )


# Board hardware does not change at runtime, so a single cached value is reused
# for the service lifetime. invalidate_board_info_cache() clears it for tests
# or in case the override file is changed by an operator action.
_BOARD_INFO_CACHE: BoardInfo | None = None
_BOARD_INFO_CACHE_LOCK = threading.Lock()


def invalidate_board_info_cache() -> None:
    """Clear the cached board detection result."""
    global _BOARD_INFO_CACHE
    with _BOARD_INFO_CACHE_LOCK:
        _BOARD_INFO_CACHE = None


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

    # 2. Device tree detection
    model_string = _read_device_model()
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
