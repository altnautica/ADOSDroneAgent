"""Tests for the displays section of the HAL board profile schema.

Covers the DisplayBinding / DisplayGpio / DisplaysSection models, the
forward-compatibility default that lets pre-existing board YAMLs absorb
the new field without modification, and the two boards that ship with a
populated displays list (Cubie A7Z and Rock 5C Lite).

Also runs an opportunistic ``dtc`` syntax check against the repo-shipped
overlay sources when the ``dtc`` binary is available on the test host.
The compile is skipped (not failed) when ``dtc`` is missing so CI on a
container without device-tree-compiler installed still passes.
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest
import yaml

from ados.hal.detect import (
    BOARDS_DIR,
    BoardProfile,
    DisplayBinding,
    DisplayGpio,
    DisplaysSection,
    _load_board_profiles,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
OVERLAY_DIR = REPO_ROOT / "data" / "overlays"
OVERLAY_DTS_FILES = sorted(p for p in OVERLAY_DIR.glob("*.dts") if p.is_file())
UPSTREAM_DTS_DIR = OVERLAY_DIR / "upstream"
UPSTREAM_DTS_FILES = sorted(p for p in UPSTREAM_DTS_DIR.glob("*.dts") if p.is_file())


# ---------------------------------------------------------------------------
# Schema defaults — every existing YAML must still load
# ---------------------------------------------------------------------------
class TestDisplaysDefaultEmpty:
    def test_default_factory_returns_empty(self):
        section = DisplaysSection()
        assert section.supported == []

    def test_board_profile_without_displays_field(self):
        """A YAML that does not mention ``displays`` should still load."""
        minimal = {
            "name": "test-board",
            "vendor": "ACME",
            "soc": "TEST",
            "arch": "aarch64",
        }
        profile = BoardProfile(**minimal)
        assert isinstance(profile.displays, DisplaysSection)
        assert profile.displays.supported == []

    def test_all_existing_profiles_carry_displays_section(self):
        """Every YAML on disk parses cleanly and exposes ``displays``."""
        profiles = _load_board_profiles()
        assert profiles, "expected at least one board profile to load"
        for profile in profiles:
            assert isinstance(profile.displays, DisplaysSection)


# ---------------------------------------------------------------------------
# Boards that DO declare a display
# ---------------------------------------------------------------------------
def _load_yaml(filename: str) -> BoardProfile:
    path = BOARDS_DIR / filename
    with open(path) as fh:
        data = yaml.safe_load(fh)
    return BoardProfile(**data)


class TestCubieA7zDisplay:
    @pytest.fixture(scope="class")
    def profile(self) -> BoardProfile:
        return _load_yaml("cubie-a7z.yaml")

    def test_has_one_display(self, profile: BoardProfile):
        assert len(profile.displays.supported) == 1

    def test_waveshare35a_metadata(self, profile: BoardProfile):
        binding = profile.displays.supported[0]
        assert isinstance(binding, DisplayBinding)
        assert binding.id == "waveshare35a"
        assert binding.type == "spi-lcd"
        assert binding.controller == "ILI9486"
        assert binding.touch_chip == "ADS7846"
        assert binding.bus == "spi1"
        assert binding.resolution == "480x320"
        assert binding.overlay_source == "repo"
        assert binding.overlay_ref == "cubie-a7z-waveshare35a.dts"
        assert binding.default_rotation == 90
        assert "fbtft" in binding.modules_required
        assert "fb_ili9486" in binding.modules_required
        assert "ads7846" in binding.modules_required

    def test_gpio_dc_pj25_pin18(self, profile: BoardProfile):
        gpio = profile.displays.supported[0].gpio["dc"]
        assert isinstance(gpio, DisplayGpio)
        assert gpio.pin == "PJ25"
        assert gpio.pinctrl == "default"
        assert gpio.header_pin == 18
        assert gpio.direction == "out"

    def test_gpio_reset_pl5_pin22_on_r_pio(self, profile: BoardProfile):
        gpio = profile.displays.supported[0].gpio["reset"]
        assert gpio.pin == "PL5"
        # PL pins live on the AO/RTC pinctrl block
        assert gpio.pinctrl == "r_pio"
        assert gpio.header_pin == 22

    def test_gpio_irq_pb1_pin11(self, profile: BoardProfile):
        gpio = profile.displays.supported[0].gpio["irq"]
        assert gpio.pin == "PB1"
        assert gpio.pinctrl == "default"
        assert gpio.header_pin == 11
        assert gpio.direction == "in"

    def test_gpio_cs_touch_pd14_pin26(self, profile: BoardProfile):
        gpio = profile.displays.supported[0].gpio["cs_touch"]
        assert gpio.pin == "PD14"
        assert gpio.header_pin == 26


class TestRock5cLiteDisplay:
    @pytest.fixture(scope="class")
    def profile(self) -> BoardProfile:
        return _load_yaml("rock-5c-lite.yaml")

    def test_has_one_display(self, profile: BoardProfile):
        assert len(profile.displays.supported) == 1

    def test_waveshare35a_metadata(self, profile: BoardProfile):
        binding = profile.displays.supported[0]
        assert binding.id == "waveshare35a"
        assert binding.type == "spi-lcd"
        assert binding.controller == "ILI9486"
        assert binding.touch_chip == "ADS7846"
        assert binding.bus == "spi4"
        # The 5C path activates the BSP-shipped DTBO, falling back to the
        # vendored upstream source if the BSP overlay package is absent.
        assert binding.overlay_source == "upstream"
        assert binding.overlay_ref == "rk3588-spi4-m2-cs0-waveshare35"

    def test_gpio_pi_pinout_landings(self, profile: BoardProfile):
        gpio_map = profile.displays.supported[0].gpio
        assert gpio_map["dc"].pin == "GPIO1_B0"
        assert gpio_map["dc"].header_pin == 18
        assert gpio_map["reset"].pin == "GPIO1_B5"
        assert gpio_map["reset"].header_pin == 22
        assert gpio_map["irq"].pin == "GPIO4_B3"
        assert gpio_map["irq"].header_pin == 11
        assert gpio_map["cs_touch"].pin == "GPIO1_A4"
        assert gpio_map["cs_touch"].header_pin == 26


# ---------------------------------------------------------------------------
# Cross-board touch invariants — both boards must report touch enabled
# ---------------------------------------------------------------------------
@pytest.mark.parametrize(
    "filename",
    ["cubie-a7z.yaml", "rock-5c-lite.yaml"],
)
class TestTouchEnabledCrossBoard:
    def test_touch_chip_present(self, filename: str):
        binding = _load_yaml(filename).displays.supported[0]
        assert binding.touch_chip == "ADS7846"

    def test_irq_pin_declared(self, filename: str):
        gpio_map = _load_yaml(filename).displays.supported[0].gpio
        assert "irq" in gpio_map
        assert gpio_map["irq"].direction == "in"

    def test_cs_touch_pin_declared(self, filename: str):
        gpio_map = _load_yaml(filename).displays.supported[0].gpio
        assert "cs_touch" in gpio_map


# ---------------------------------------------------------------------------
# Overlay DTS files — opportunistic ``dtc`` syntax check
# ---------------------------------------------------------------------------
def _has_dtc() -> bool:
    return shutil.which("dtc") is not None


@pytest.mark.skipif(not _has_dtc(), reason="device-tree-compiler not installed")
class TestOverlayDtcCompiles:
    @pytest.mark.parametrize("dts", OVERLAY_DTS_FILES, ids=lambda p: p.name)
    def test_repo_overlay_compiles(self, dts: Path, tmp_path: Path):
        """Each repo-shipped DTS in data/overlays/ must compile via dtc.

        Uses ``-@`` to allow phandle references and ``-Hepapr`` defaults.
        Failures here mean the overlay source has a syntax bug that
        would break ``install-display-overlay.sh`` at install time.
        """
        out = tmp_path / f"{dts.stem}.dtbo"
        result = subprocess.run(
            ["dtc", "-@", "-I", "dts", "-O", "dtb", "-o", str(out), str(dts)],
            capture_output=True,
            text=True,
        )
        assert result.returncode == 0, (
            f"dtc failed for {dts.name}:\nSTDOUT:\n{result.stdout}\n"
            f"STDERR:\n{result.stderr}"
        )
        assert out.exists() and out.stat().st_size > 0

    @pytest.mark.parametrize("dts", UPSTREAM_DTS_FILES, ids=lambda p: p.name)
    def test_upstream_overlay_compiles(self, dts: Path, tmp_path: Path):
        """Vendored upstream DTS sources must also compile.

        These rely on Rockchip dt-bindings headers which dtc cannot
        resolve without the kernel header search path. We feed dtc the
        preprocessed input via cpp so the bindings macros expand cleanly.
        """
        cpp = shutil.which("cpp") or shutil.which("gcc")
        if not cpp:
            pytest.skip("cpp not available for preprocessor pass")
        preprocessed = tmp_path / f"{dts.stem}.preprocessed.dts"
        cpp_cmd = [cpp, "-E", "-x", "assembler-with-cpp", "-undef", "-nostdinc"]
        # The Rockchip + GPIO + IRQ bindings ship with the kernel headers
        # package. Skip if unavailable rather than fail.
        kernel_includes = [
            "/usr/include",
            "/usr/src/linux-headers-$(uname -r)/include",
        ]
        for inc in kernel_includes:
            cpp_cmd += ["-I", inc]
        cpp_cmd += [str(dts), "-o", str(preprocessed)]
        cpp_result = subprocess.run(cpp_cmd, capture_output=True, text=True)
        if cpp_result.returncode != 0:
            pytest.skip(
                "cpp could not resolve dt-bindings headers; "
                "kernel-headers package likely absent on this host"
            )
        out = tmp_path / f"{dts.stem}.dtbo"
        dtc_result = subprocess.run(
            ["dtc", "-@", "-I", "dts", "-O", "dtb", "-o", str(out), str(preprocessed)],
            capture_output=True,
            text=True,
        )
        assert dtc_result.returncode == 0, (
            f"dtc failed for {dts.name}:\nSTDOUT:\n{dtc_result.stdout}\n"
            f"STDERR:\n{dtc_result.stderr}"
        )
        assert out.exists() and out.stat().st_size > 0


# ---------------------------------------------------------------------------
# Sanity — the repo overlay file referenced by cubie-a7z.yaml exists
# ---------------------------------------------------------------------------
def test_cubie_a7z_overlay_file_exists():
    binding = _load_yaml("cubie-a7z.yaml").displays.supported[0]
    overlay = OVERLAY_DIR / binding.overlay_ref
    assert overlay.exists(), f"missing overlay source at {overlay}"


def test_upstream_dts_file_exists():
    """The vendored fallback for the 5C must be present."""
    expected = UPSTREAM_DTS_DIR / "rk3588-spi4-m2-cs0-waveshare35.dts"
    assert expected.exists(), f"missing vendored upstream DTS at {expected}"
