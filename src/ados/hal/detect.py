"""Hardware Abstraction Layer — board detection and profiling."""

from __future__ import annotations

import platform
from dataclasses import dataclass
from pathlib import Path

import yaml

from ados.core.logging import get_logger

log = get_logger("hal")

BOARDS_DIR = Path(__file__).parent / "boards"


@dataclass
class BoardInfo:
    name: str
    model: str
    tier: int
    ram_mb: int
    cpu_cores: int

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "model": self.model,
            "tier": self.tier,
            "ram_mb": self.ram_mb,
            "cpu_cores": self.cpu_cores,
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


def _load_board_profiles() -> list[dict]:
    """Load all YAML board profiles."""
    profiles = []
    if BOARDS_DIR.is_dir():
        for yaml_file in sorted(BOARDS_DIR.glob("*.yaml")):
            with open(yaml_file) as f:
                data = yaml.safe_load(f)
                if data:
                    profiles.append(data)
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


def detect_board() -> BoardInfo:
    """Detect the current board by reading /proc/device-tree/model
    and matching against board profiles."""
    import psutil

    model_string = _read_device_model()
    ram_mb = psutil.virtual_memory().total // (1024 * 1024)
    cpu_cores = psutil.cpu_count(logical=True) or 1

    profiles = _load_board_profiles()

    for profile in profiles:
        patterns = profile.get("model_patterns", [])
        for pattern in patterns:
            if pattern.lower() in model_string.lower():
                tier = profile.get("default_tier", detect_tier(ram_mb))
                board = BoardInfo(
                    name=profile.get("name", "unknown"),
                    model=model_string or profile.get("name", "unknown"),
                    tier=tier,
                    ram_mb=ram_mb,
                    cpu_cores=cpu_cores,
                )
                log.info("board_detected", board=board.name, tier=board.tier, ram_mb=ram_mb)
                return board

    # Fallback — use platform info for a sensible name
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
        model=model_string or f"{system} {machine}",
        tier=tier,
        ram_mb=ram_mb,
        cpu_cores=cpu_cores,
    )
    log.info("board_fallback", board=board.name, tier=board.tier, ram_mb=ram_mb)
    return board
