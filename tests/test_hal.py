"""Tests for HAL board detection — profiles, fingerprint collisions, override, fallback."""

from __future__ import annotations

import platform
from unittest.mock import patch

import pytest

from ados.hal.detect import (
    BOARDS_DIR,
    BoardInfo,
    BoardProfile,
    _load_board_profiles,
    detect_board,
    detect_board_profile,
    detect_tier,
    invalidate_board_info_cache,
    known_board_stems,
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
# One real device-tree fingerprint per board: (compatible first token, model
# string, online cpu count). These are what the kernel actually reports, so a
# resolution that gets one wrong gets a real board wrong.
DEVICE_TREE_STRINGS: dict[str, str] = {
    "Raspberry Pi CM4": "Raspberry Pi Compute Module 4 Rev 1.0",
    "Raspberry Pi CM5": "Raspberry Pi Compute Module 5 Rev 1.0",
    "Raspberry Pi 4B": "Raspberry Pi 4 Model B Rev 1.4",
    "Raspberry Pi 5": "Raspberry Pi 5 Model B Rev 1.0",
    "NVIDIA Jetson Nano": "NVIDIA Jetson Nano Developer Kit",
    "NVIDIA Jetson Orin Nano": "NVIDIA Jetson Orin Nano Developer Kit",
    "Orange Pi 5": "Orange Pi 5 Board V1.2",
    "Radxa CM3 (RK3566)": "Radxa CM3 IO Board",
    "Raspberry Pi 3": "Raspberry Pi 3 Model B Plus Rev 1.3",
    "Raspberry Pi Compute Module 3": "Raspberry Pi Compute Module 3 Plus Rev 1.0",
}


def resolve_board(
    compatible: str = "",
    model: str = "",
    cpuinfo: str = "",
    override: str = "",
    cpu_cores: int = 4,
    ram_gb: int = 8,
    system: str = "Linux",
    machine: str = "aarch64",
) -> BoardInfo:
    """Run the real detection pipeline over injected host facts.

    Everything below asserts on the RESOLVED board — the value that reaches
    /run/ados/board.json, /api/status and the FC-link probe — rather than on
    the fingerprint patterns a profile happens to list. The platform is pinned
    to an arm64 Linux SBC by default so a resolution assertion cannot depend on
    the machine running the suite.
    """
    invalidate_board_info_cache()
    with (
        patch("ados.hal.detect._read_board_override", return_value=override),
        patch("ados.hal.detect._read_device_compatible", return_value=compatible),
        patch("ados.hal.detect._read_device_model", return_value=model),
        patch("ados.hal.detect._read_cpuinfo_model", return_value=cpuinfo),
        patch("ados.hal.detect.platform.system", return_value=system),
        patch("ados.hal.detect.platform.machine", return_value=machine),
        patch("psutil.virtual_memory") as mock_mem,
        patch("psutil.cpu_count", return_value=cpu_cores),
    ):
        mock_mem.return_value = type(
            "VMem", (), {"total": ram_gb * 1024 * 1024 * 1024}
        )()
        return detect_board(force=True)


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
    def test_every_yaml_is_present_and_stems_are_the_override_grammar(self):
        """All YAML files must be present, and each must expose its stem."""
        actual = sorted(f.name for f in BOARDS_DIR.glob("*.yaml"))
        assert actual == EXPECTED_PROFILES
        # Sanity: at least the original 9 baseline boards are present.
        assert len(actual) >= 9
        # Every profile carries the filename stem the override grammar uses.
        assert known_board_stems() == sorted(f.stem for f in BOARDS_DIR.glob("*.yaml"))

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
# Fingerprint collisions — asserted on the RESOLVED board, not on patterns
#
# The previous version of this class compared pattern STRINGS across profiles
# and required them to be byte-distinct. That is structurally incapable of
# seeing the three collisions that actually shipped: a shared SoC-family name
# (`sun60iw2`, listed by the A7Z while being the A7S's own model string), two
# whole profiles claiming one SoC with different `npu_tops` and UART sets, and
# two board variants behind ONE device tree. Each of those resolves through
# substring matching and filename order, which no pattern-equality check can
# observe. So the assertions below run the real pipeline over real kernel
# fingerprints and compare the board that comes out.
# ---------------------------------------------------------------------------
class TestFingerprintCollisions:
    def test_no_two_profiles_claim_the_same_pattern(self):
        """A cheap structural guard, kept: a literally duplicated pattern."""
        profiles = _load_board_profiles()
        pattern_owners: dict[str, str] = {}
        for profile in profiles:
            seen_in_profile: set[str] = set()
            for pat in profile.model_patterns:
                pat_lower = pat.lower()
                if pat_lower in seen_in_profile:
                    continue
                seen_in_profile.add(pat_lower)
                if (
                    pat_lower in pattern_owners
                    and pattern_owners[pat_lower] != profile.name
                ):
                    raise AssertionError(
                        f"Pattern '{pat}' claimed by both '{pattern_owners[pat_lower]}' "
                        f"and '{profile.name}'"
                    )
                pattern_owners[pat_lower] = profile.name

    @pytest.mark.parametrize(
        "compatible,model,cpu_cores,expected",
        [
            # The A733 pair: same SoC, same model string, different boards.
            ("radxa,cubie-a7s", "sun60iw2", 8, "Radxa Cubie A7S"),
            ("radxa,cubie-a7z", "sun60iw2", 8, "Radxa Cubie A7Z"),
            # The ROCK 5C pair: ONE device tree, two SoC bins.
            ("radxa,rock-5c", "Radxa ROCK 5C ", 6, "Radxa ROCK 5C Lite (RK3582)"),
            ("radxa,rock-5c", "Radxa ROCK 5C ", 8, "Radxa ROCK 5C (RK3588S2)"),
            # RK3566: one profile now, reachable by board name and by SoC.
            ("radxa,cm3", "Radxa CM3 IO Board", 4, "Radxa CM3 (RK3566)"),
            ("rockchip,rk3566", "RK3566 EVB", 4, "Radxa CM3 (RK3566)"),
            ("radxa,cm4", "Radxa CM4", 8, "Radxa CM4 (RK3588S2)"),
            # The BCM2837 pair: shared SoC, distinct model strings.
            (
                "raspberrypi,3-model-b-plus",
                "Raspberry Pi 3 Model B Plus Rev 1.3",
                4,
                "Raspberry Pi 3",
            ),
            (
                "raspberrypi,3-model-b",
                "Raspberry Pi 3 Model B Rev 1.2",
                4,
                "Raspberry Pi 3",
            ),
            (
                "raspberrypi,3-compute-module",
                "Raspberry Pi Compute Module 3 Plus Rev 1.0",
                4,
                "Raspberry Pi Compute Module 3",
            ),
            (
                "raspberrypi,4-model-b",
                "Raspberry Pi 4 Model B Rev 1.4",
                4,
                "Raspberry Pi 4B",
            ),
        ],
    )
    def test_real_device_tree_fingerprints_resolve_to_one_board(
        self, compatible: str, model: str, cpu_cores: int, expected: str
    ):
        board = resolve_board(
            compatible=compatible, model=model, cpu_cores=cpu_cores
        )
        assert board.name == expected

    def test_a_shared_soc_family_string_alone_identifies_no_board(self):
        """`sun60iw2` names the A733 SoC, which both Cubie boards carry.

        The A7Z used to list it, so a real A7S whose compatible node could not
        be read came up as an A7Z: the wrong FC UART (/dev/ttyS3 vs ttyS2), a
        40-pin Pi GPIO map it does not have, and no local ONNX inference. It
        must resolve to neither board.
        """
        board = resolve_board(model="sun60iw2")
        assert board.name == "generic-arm64", board

    def test_the_rk3566_board_keeps_its_npu_and_codecs(self):
        """Two profiles claimed RK3566 with npu_tops 0.0 and 0.8.

        `has_accelerator` and the perception tier therefore flipped on identical
        silicon depending on which filename sorted first. One profile survives,
        and it is the measured one.
        """
        board = resolve_board(compatible="radxa,cm3", model="Radxa CM3 IO Board")
        assert board.npu_tops == 0.8
        assert board.has_accelerator
        assert "vp9_dec" in board.hw_video_codecs

    def test_the_rock_5c_variant_publishes_its_own_soc(self):
        """The full 5C published the Lite's RK3582 all the way to the operator."""
        lite = resolve_board(
            compatible="radxa,rock-5c", model="Radxa ROCK 5C ", cpu_cores=6
        )
        full = resolve_board(
            compatible="radxa,rock-5c", model="Radxa ROCK 5C ", cpu_cores=8
        )
        assert lite.soc == "RK3582"
        assert full.soc == "RK3588S2"
        # Same PCB: the NPU and the perception tier do not change with the bin.
        assert lite.npu_tops == full.npu_tops
        assert lite.has_accelerator and full.has_accelerator


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
        board = resolve_board(model=dt_string, ram_gb=4)
        assert board.name == board_name
        assert board.tier >= 1


# ---------------------------------------------------------------------------
# Compatible-node disambiguation: the Allwinner A733 boards (Cubie A7S / A7Z)
# share the generic device-tree model "sun60iw2", so detection must fall to the
# unique first token of /proc/device-tree/compatible.
# ---------------------------------------------------------------------------
class TestDetectBoardCompatible:
    def test_the_compatible_token_is_preferred_over_the_model_string(self):
        """The A7S resolves from its compatible token, not from `sun60iw2`.

        Neither profile may list the shared A733 family string, so the
        compatible token is the only thing that names the board — and it must be
        consulted before the model string, which is the SoC's name here.
        """
        board = resolve_board(
            compatible="radxa,cubie-a7s", model="sun60iw2", cpu_cores=8, ram_gb=4
        )
        assert board.name == "Radxa Cubie A7S"
        assert board.soc == "Allwinner A733"
        # The detected model string is recorded as measured, not replaced.
        assert board.model == "sun60iw2"

    def test_empty_compatible_falls_through_to_model(self):
        """No compatible node -> detection still works via the model string."""
        board = resolve_board(model="Raspberry Pi 4 Model B Rev 1.4", ram_gb=4)
        assert board.name == "Raspberry Pi 4B"


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
            patch("ados.hal.detect._read_device_compatible", return_value=""),
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
# Board override: the grammar is the YAML filename STEM
#
# It used to be matched against `profile.name`, the display name — which the
# setup facade's slug validator rejects for 20 of the 21 profiles, so the
# documented escape hatch for a mis-detected board could not be used at all.
# ---------------------------------------------------------------------------
class TestBoardOverride:
    @pytest.mark.parametrize("stem", sorted(f.stem for f in BOARDS_DIR.glob("*.yaml")))
    def test_every_stem_resolves_to_its_own_profile(self, stem: str):
        """Asserted over EVERY board so a new profile cannot ship unreachable.

        The Rust sidecar writer asserts the same property over the same
        directory (`board_sidecar::every_board_stem_is_a_resolvable_override_token`),
        which is what makes one token name one board in both halves — one of
        them writes /run/ados/board.json, the other answers the Python services.
        """
        profiles = {p.stem: p for p in _load_board_profiles()}
        expected = profiles[stem]
        board = resolve_board(override=stem, cpu_cores=4)
        # cpu_cores=4 selects no variant in any shipped profile, so the
        # profile's own name is the expected one.
        assert board.name == expected.name
        assert board.soc == expected.soc
        assert board.npu_tops == expected.compute.npu_tops

    def test_the_stem_pins_a_board_its_fingerprint_cannot_name(self):
        """`cubie-a7s` is a legal slug; "Radxa Cubie A7S" never was.

        This is the case the override exists for: an A7S whose compatible node
        reads only the shared A733 family string.
        """
        board = resolve_board(override="cubie-a7s", model="sun60iw2", cpu_cores=8)
        assert board.name == "Radxa Cubie A7S"
        assert board.soc == "Allwinner A733"
        # The measured model string survives; the token does not replace it.
        assert board.model == "sun60iw2"

    def test_override_takes_priority_over_the_device_tree(self):
        board = resolve_board(
            override="cm4", model="NVIDIA Jetson Nano Developer Kit", ram_gb=4
        )
        assert board.name == "Raspberry Pi CM4"

    def test_an_unresolvable_override_falls_through_to_auto_detection(self):
        """A typo must not cost the node its profile.

        The override corrects a mis-detection; a token naming no profile used to
        mint a bare record with that token as the board name — no UART
        candidates, no codecs, no perception tier — which is strictly worse than
        what the hardware itself reports.
        """
        board = resolve_board(
            override="Radxa ROCK 5C Lite (RK3582)",
            model="Orange Pi 5 Board V1.2",
            cpu_cores=8,
        )
        assert board.name == "Orange Pi 5"

    def test_empty_override_ignored(self):
        board = resolve_board(model="Orange Pi 5 Board V1.2", cpu_cores=8)
        assert board.name == "Orange Pi 5"


# ---------------------------------------------------------------------------
# /proc/cpuinfo fallback + unknown-board degradation
# ---------------------------------------------------------------------------
class TestCpuinfoFallback:
    def test_cpuinfo_fallback_when_device_tree_fails(self):
        """When device-tree is empty, cpuinfo model should be used for matching."""
        board = resolve_board(cpuinfo="Radxa CM3 SoM Board", ram_gb=2)
        assert board.name == "Radxa CM3 (RK3566)"
        assert board.vendor == "Radxa"

    def test_cpuinfo_not_used_when_device_tree_matches(self):
        """If device-tree already matches, cpuinfo should not override."""
        board = resolve_board(
            model="Raspberry Pi 4 Model B Rev 1.4",
            cpuinfo="NVIDIA Jetson Nano",
            ram_gb=4,
        )
        assert board.name == "Raspberry Pi 4B"

    def test_an_unknown_board_resolves_through_the_generic_profile(self):
        """Parity with the Rust sidecar writer, which does the same.

        This half used to mint a bare `generic-<machine>` record that matches no
        profile, so the FC-link probe had no fallback UART candidates to offer
        on exactly the hardware that needs them most. `platform.machine` is
        pinned so the assertion does not depend on the host running the suite.
        """
        invalidate_board_info_cache()
        with (
            patch("ados.hal.detect._read_board_override", return_value=""),
            patch("ados.hal.detect._read_device_compatible", return_value=""),
            patch("ados.hal.detect._read_device_model", return_value=""),
            patch(
                "ados.hal.detect._read_cpuinfo_model", return_value="Unknown Board XYZ"
            ),
            patch("ados.hal.detect.platform.machine", return_value="aarch64"),
            patch("ados.hal.detect.platform.system", return_value="Linux"),
            patch("psutil.virtual_memory") as mock_mem,
            patch("psutil.cpu_count", return_value=4),
        ):
            mock_mem.return_value = type("VMem", (), {"total": 4 * 1024**3})()
            board = detect_board(force=True)
            assert board.name == "generic-arm64"
            # The measured model string is kept, and the tier comes from real RAM.
            assert board.model == "Unknown Board XYZ"
            assert board.tier == detect_tier(4096)

            profile = detect_board_profile(force=True)
            assert profile is not None
            assert profile.stem == "generic-arm64"
            assert profile.uart_paths, "the generic profile must carry fallback UARTs"


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
