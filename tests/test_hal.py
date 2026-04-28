"""Tests for HAL board detection — covers all 9 board profiles, override, cpuinfo fallback."""

from __future__ import annotations

import platform
from unittest.mock import patch

import pytest

from ados.hal.detect import (
    BOARDS_DIR,
    BoardInfo,
    BoardProfile,
    _load_board_profiles,
    _match_profile,
    detect_board,
    detect_tier,
    invalidate_board_info_cache,
)


@pytest.fixture(autouse=True)
def _clear_board_cache():
    """Clear cached board info before and after each test."""
    invalidate_board_info_cache()
    yield
    invalidate_board_info_cache()

# ---------------------------------------------------------------------------
# YAML profile filenames discovered at runtime (count grows as new boards land)
# ---------------------------------------------------------------------------
EXPECTED_PROFILES = sorted(f.name for f in BOARDS_DIR.glob("*.yaml"))

# Model strings that should match each profile (device-tree style)
DEVICE_TREE_STRINGS: dict[str, str] = {
    "Raspberry Pi CM4": "Raspberry Pi Compute Module 4 Rev 1.0",
    "Raspberry Pi CM5": "Raspberry Pi Compute Module 5 Rev 1.0",
    "Raspberry Pi 4B": "Raspberry Pi 4 Model B Rev 1.4",
    "Raspberry Pi 5": "Raspberry Pi 5 Model B Rev 1.0",
    "NVIDIA Jetson Nano": "NVIDIA Jetson Nano Developer Kit",
    "NVIDIA Jetson Orin Nano": "NVIDIA Jetson Orin Nano Developer Kit",
    "Orange Pi 5": "Orange Pi 5 Board V1.2",
    "Radxa CM3": "Radxa CM3 IO Board",
}


# ---------------------------------------------------------------------------
# Tier detection
# ---------------------------------------------------------------------------
class TestDetectTier:
    def test_tier_1_low_ram(self):
        assert detect_tier(256) == 1

    def test_tier_2_mid_ram(self):
        assert detect_tier(512) == 2
        assert detect_tier(1024) == 2

    def test_tier_3_standard_ram(self):
        assert detect_tier(2048) == 3
        assert detect_tier(4096) == 3

    def test_tier_4_high_ram(self):
        assert detect_tier(8192) == 4


# ---------------------------------------------------------------------------
# YAML profiles load and validate via Pydantic
# ---------------------------------------------------------------------------
class TestBoardProfiles:
    def test_all_9_profiles_exist(self):
        """All YAML files must be present."""
        actual = sorted(f.name for f in BOARDS_DIR.glob("*.yaml"))
        assert actual == EXPECTED_PROFILES
        # Sanity: at least the original 9 baseline boards are present.
        assert len(actual) >= 9

    @pytest.mark.parametrize("filename", EXPECTED_PROFILES)
    def test_profile_loads_and_validates(self, filename: str):
        """Each YAML file must parse into a valid BoardProfile."""
        import yaml

        path = BOARDS_DIR / filename
        with open(path) as f:
            data = yaml.safe_load(f)
        profile = BoardProfile(**data)
        assert profile.name
        assert profile.vendor
        assert profile.soc
        assert profile.arch in ("aarch64", "armhf", "armv7l")
        assert isinstance(profile.default_tier, int)
        assert isinstance(profile.gpio_pins, list)
        assert isinstance(profile.uart_paths, list)
        assert isinstance(profile.hw_video_codecs, list)

    @pytest.mark.parametrize("filename", EXPECTED_PROFILES)
    def test_profile_has_new_fields(self, filename: str):
        """Every profile must have vendor, soc, arch, hw_video_codecs."""
        import yaml

        path = BOARDS_DIR / filename
        with open(path) as f:
            data = yaml.safe_load(f)
        assert "vendor" in data
        assert "soc" in data
        assert "arch" in data
        assert "hw_video_codecs" in data

    def test_load_board_profiles_returns_all(self):
        """_load_board_profiles returns validated BoardProfile objects for every yaml file."""
        profiles = _load_board_profiles()
        assert len(profiles) == len(EXPECTED_PROFILES)
        for p in profiles:
            assert isinstance(p, BoardProfile)

    def test_non_generic_profiles_have_codecs(self):
        """Every board except generic-arm64 should list at least one hw video codec."""
        profiles = _load_board_profiles()
        for p in profiles:
            if p.name != "generic-arm64":
                assert len(p.hw_video_codecs) > 0, f"{p.name} has no hw_video_codecs"


# ---------------------------------------------------------------------------
# Pattern overlap — no two boards match the same device-tree string
# ---------------------------------------------------------------------------
class TestPatternOverlap:
    def test_no_pattern_overlap(self):
        """No two distinct board profiles should match the same device-tree model string."""
        profiles = _load_board_profiles()
        # Collect every pattern with its board name. A profile listing the
        # same pattern twice in different cases (e.g. 'RK3576' + 'rk3576')
        # is a yaml authoring nit, not a cross-profile collision; ignore it.
        pattern_owners: dict[str, str] = {}
        for profile in profiles:
            seen_in_profile: set[str] = set()
            for pat in profile.model_patterns:
                pat_lower = pat.lower()
                if pat_lower in seen_in_profile:
                    continue
                seen_in_profile.add(pat_lower)
                if pat_lower in pattern_owners and pattern_owners[pat_lower] != profile.name:
                    raise AssertionError(
                        f"Pattern '{pat}' claimed by both '{pattern_owners[pat_lower]}' "
                        f"and '{profile.name}'"
                    )
                pattern_owners[pat_lower] = profile.name

    def test_device_tree_strings_match_unique_boards(self):
        """Each test device-tree string should match exactly one profile."""
        profiles = _load_board_profiles()
        for expected_name, dt_string in DEVICE_TREE_STRINGS.items():
            matched = _match_profile(profiles, dt_string)
            assert matched is not None, f"No profile matched '{dt_string}'"
            assert matched.name == expected_name, (
                f"Expected '{expected_name}' but got '{matched.name}' for '{dt_string}'"
            )


# ---------------------------------------------------------------------------
# detect_board() with mocked device-tree for each board
# ---------------------------------------------------------------------------
class TestDetectBoardDeviceTree:
    @pytest.mark.parametrize(
        "board_name,dt_string",
        list(DEVICE_TREE_STRINGS.items()),
    )
    def test_detect_board_from_device_tree(self, board_name: str, dt_string: str):
        """detect_board() returns correct BoardInfo when device-tree matches."""
        with (
            patch("ados.hal.detect._read_board_override", return_value=""),
            patch("ados.hal.detect._read_device_model", return_value=dt_string),
            patch("ados.hal.detect._read_cpuinfo_model", return_value=""),
            patch("psutil.virtual_memory") as mock_mem,
            patch("psutil.cpu_count", return_value=4),
        ):
            mock_mem.return_value = type("VMem", (), {"total": 4 * 1024 * 1024 * 1024})()
            board = detect_board()
            assert board.name == board_name
            assert board.tier >= 1


# ---------------------------------------------------------------------------
# BoardInfo has all new fields
# ---------------------------------------------------------------------------
class TestBoardInfoFields:
    def test_new_fields_default(self):
        info = BoardInfo(
            name="test",
            model="test model",
            tier=2,
            ram_mb=2048,
            cpu_cores=4,
        )
        assert info.vendor == "unknown"
        assert info.soc == "unknown"
        assert info.arch == "aarch64"
        assert info.hw_video_codecs == []

    def test_new_fields_set(self):
        info = BoardInfo(
            name="test",
            model="test model",
            tier=3,
            ram_mb=4096,
            cpu_cores=4,
            vendor="NVIDIA",
            soc="Tegra X1",
            arch="aarch64",
            hw_video_codecs=["h264_enc", "h264_dec"],
        )
        assert info.vendor == "NVIDIA"
        assert info.soc == "Tegra X1"
        assert info.hw_video_codecs == ["h264_enc", "h264_dec"]

    def test_to_dict_includes_new_fields(self):
        info = BoardInfo(
            name="test",
            model="m",
            tier=2,
            ram_mb=1024,
            cpu_cores=2,
            vendor="Xunlong",
            soc="RK3588S",
            arch="aarch64",
            hw_video_codecs=["h265_enc"],
        )
        d = info.to_dict()
        assert d["vendor"] == "Xunlong"
        assert d["soc"] == "RK3588S"
        assert d["arch"] == "aarch64"
        assert d["hw_video_codecs"] == ["h265_enc"]

    def test_detect_board_returns_new_fields(self):
        """When a profile matches, BoardInfo should carry vendor/soc/arch/codecs."""
        dt_string = "NVIDIA Jetson Orin Nano Developer Kit"
        with (
            patch("ados.hal.detect._read_board_override", return_value=""),
            patch("ados.hal.detect._read_device_model", return_value=dt_string),
            patch("ados.hal.detect._read_cpuinfo_model", return_value=""),
            patch("psutil.virtual_memory") as mock_mem,
            patch("psutil.cpu_count", return_value=6),
        ):
            mock_mem.return_value = type("VMem", (), {"total": 8 * 1024 * 1024 * 1024})()
            board = detect_board()
            assert board.vendor == "NVIDIA"
            assert board.soc == "Tegra Orin"
            assert board.arch == "aarch64"
            assert "h264_enc" in board.hw_video_codecs
            assert "av1_dec" in board.hw_video_codecs


# ---------------------------------------------------------------------------
# Board override file mechanism
# ---------------------------------------------------------------------------
class TestBoardOverride:
    def test_override_matches_profile(self):
        """Override file with a known profile name loads that profile."""
        with (
            patch("ados.hal.detect._read_board_override", return_value="Raspberry Pi CM5"),
            patch("ados.hal.detect._read_device_model", return_value=""),
            patch("ados.hal.detect._read_cpuinfo_model", return_value=""),
            patch("psutil.virtual_memory") as mock_mem,
            patch("psutil.cpu_count", return_value=4),
        ):
            mock_mem.return_value = type("VMem", (), {"total": 4 * 1024 * 1024 * 1024})()
            board = detect_board()
            assert board.name == "Raspberry Pi CM5"
            assert board.vendor == "Raspberry Pi"
            assert board.soc == "BCM2712"
            assert board.tier == 4

    def test_override_unmatched_name(self):
        """Override file with unknown name still uses it as board name."""
        with (
            patch("ados.hal.detect._read_board_override", return_value="Custom Board X"),
            patch("ados.hal.detect._read_device_model", return_value=""),
            patch("ados.hal.detect._read_cpuinfo_model", return_value=""),
            patch("psutil.virtual_memory") as mock_mem,
            patch("psutil.cpu_count", return_value=2),
        ):
            mock_mem.return_value = type("VMem", (), {"total": 1024 * 1024 * 1024})()
            board = detect_board()
            assert board.name == "Custom Board X"

    def test_override_takes_priority_over_device_tree(self):
        """Override file should be checked before device-tree."""
        with (
            patch("ados.hal.detect._read_board_override", return_value="Raspberry Pi CM4"),
            patch(
                "ados.hal.detect._read_device_model",
                return_value="NVIDIA Jetson Nano Developer Kit",
            ),
            patch("ados.hal.detect._read_cpuinfo_model", return_value=""),
            patch("psutil.virtual_memory") as mock_mem,
            patch("psutil.cpu_count", return_value=4),
        ):
            mock_mem.return_value = type("VMem", (), {"total": 4 * 1024 * 1024 * 1024})()
            board = detect_board()
            # Override wins, not device-tree
            assert board.name == "Raspberry Pi CM4"

    def test_empty_override_ignored(self):
        """Empty override file should fall through to device-tree."""
        with (
            patch("ados.hal.detect._read_board_override", return_value=""),
            patch(
                "ados.hal.detect._read_device_model",
                return_value="Orange Pi 5 Board V1.2",
            ),
            patch("ados.hal.detect._read_cpuinfo_model", return_value=""),
            patch("psutil.virtual_memory") as mock_mem,
            patch("psutil.cpu_count", return_value=8),
        ):
            mock_mem.return_value = type("VMem", (), {"total": 8 * 1024 * 1024 * 1024})()
            board = detect_board()
            assert board.name == "Orange Pi 5"


# ---------------------------------------------------------------------------
# /proc/cpuinfo fallback
# ---------------------------------------------------------------------------
class TestCpuinfoFallback:
    def test_cpuinfo_fallback_when_device_tree_fails(self):
        """When device-tree is empty, cpuinfo model should be used for matching."""
        with (
            patch("ados.hal.detect._read_board_override", return_value=""),
            patch("ados.hal.detect._read_device_model", return_value=""),
            patch(
                "ados.hal.detect._read_cpuinfo_model",
                return_value="Radxa CM3 SoM Board",
            ),
            patch("psutil.virtual_memory") as mock_mem,
            patch("psutil.cpu_count", return_value=4),
        ):
            mock_mem.return_value = type("VMem", (), {"total": 2 * 1024 * 1024 * 1024})()
            board = detect_board()
            assert board.name == "Radxa CM3"
            assert board.vendor == "Radxa"

    def test_cpuinfo_not_used_when_device_tree_matches(self):
        """If device-tree already matches, cpuinfo should not override."""
        with (
            patch("ados.hal.detect._read_board_override", return_value=""),
            patch(
                "ados.hal.detect._read_device_model",
                return_value="Raspberry Pi 4 Model B Rev 1.4",
            ),
            patch(
                "ados.hal.detect._read_cpuinfo_model",
                return_value="NVIDIA Jetson Nano",
            ),
            patch("psutil.virtual_memory") as mock_mem,
            patch("psutil.cpu_count", return_value=4),
        ):
            mock_mem.return_value = type("VMem", (), {"total": 4 * 1024 * 1024 * 1024})()
            board = detect_board()
            assert board.name == "Raspberry Pi 4B"

    def test_cpuinfo_no_match_falls_to_platform(self):
        """When cpuinfo also fails to match, platform fallback is used."""
        with (
            patch("ados.hal.detect._read_board_override", return_value=""),
            patch("ados.hal.detect._read_device_model", return_value=""),
            patch("ados.hal.detect._read_cpuinfo_model", return_value="Unknown Board XYZ"),
            patch("psutil.virtual_memory") as mock_mem,
            patch("psutil.cpu_count", return_value=4),
        ):
            mock_mem.return_value = type("VMem", (), {"total": 4 * 1024 * 1024 * 1024})()
            board = detect_board()
            # Should fall through to platform-based fallback
            assert "macOS" in board.name or "generic" in board.name


# ---------------------------------------------------------------------------
# Existing tests preserved (from test_hal_macos.py)
# ---------------------------------------------------------------------------
class TestDetectBoardBasic:
    def test_detect_board_doesnt_crash(self):
        """detect_board() should return a BoardInfo on any platform."""
        board = detect_board()
        assert isinstance(board, BoardInfo)
        assert board.name != ""
        assert board.ram_mb > 0
        assert board.cpu_cores >= 1
        assert board.tier >= 1

    def test_detect_board_macos_name(self):
        """On macOS, fallback should say 'macOS (dev)', not 'generic-arm64'."""
        if platform.system() != "Darwin":
            return
        board = detect_board()
        assert "macOS" in board.name or "generic" in board.name
        assert board.name != "generic-arm64"
