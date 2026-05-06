"""Unit tests for the manifest emitter that powers the Mission Control
flash tool. Covers arch normalization, stack resolution rules, install
tier detection, install command shape per stack, and board projection
edge cases. The tests use plain dicts as fixtures so we don't depend on
any real YAML on disk.

Run from the agent repo root:

    python3 -m pytest agents/lite-rs/test_emit_manifest.py -v
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import emit_ados_agent_manifest as emit  # noqa: E402


class MapArchTests(unittest.TestCase):
    def test_armv7l_maps_to_armv7_musl(self) -> None:
        self.assertEqual(emit.map_arch("armv7l"), "armv7-musl")

    def test_aarch64_maps_to_aarch64_glibc(self) -> None:
        self.assertEqual(emit.map_arch("aarch64"), "aarch64-glibc")

    def test_arm64_alias_maps_to_aarch64_glibc(self) -> None:
        self.assertEqual(emit.map_arch("arm64"), "aarch64-glibc")

    def test_armv7_alias_maps_to_armv7_musl(self) -> None:
        self.assertEqual(emit.map_arch("armv7"), "armv7-musl")

    def test_none_falls_back_to_aarch64_glibc(self) -> None:
        self.assertEqual(emit.map_arch(None), "aarch64-glibc")

    def test_unknown_arch_falls_back_to_aarch64_glibc(self) -> None:
        self.assertEqual(emit.map_arch("riscv64"), "aarch64-glibc")


class ResolveStacksTests(unittest.TestCase):
    def test_low_ram_board_without_ground_station_is_drone_only(self) -> None:
        board = {
            "compute": {"ram_mb": 256},
            "profiles": {"tier": "budget"},
        }
        self.assertEqual(emit.resolve_stacks(board), ["ados-drone-agent"])

    def test_explicit_ground_station_block_unlocks_ground_agent(self) -> None:
        board = {
            "compute": {"ram_mb": 1024},
            "profiles": {"ground_station": {"tier": "standard"}},
        }
        self.assertEqual(
            emit.resolve_stacks(board),
            ["ados-drone-agent", "ados-ground-agent"],
        )

    def test_high_ram_board_without_ground_block_still_unlocks_ground(self) -> None:
        board = {
            "compute": {"ram_mb": 2048},
            "profiles": {},
        }
        self.assertEqual(
            emit.resolve_stacks(board),
            ["ados-drone-agent", "ados-ground-agent"],
        )

    def test_missing_compute_block_treated_as_low_ram(self) -> None:
        board = {"profiles": {}}
        self.assertEqual(emit.resolve_stacks(board), ["ados-drone-agent"])


class ResolveInstallTierTests(unittest.TestCase):
    def test_profiles_tier_budget_is_returned(self) -> None:
        board = {"profiles": {"tier": "budget"}}
        self.assertEqual(emit.resolve_install_tier(board), "budget")

    def test_drone_agent_tier_lite_is_returned(self) -> None:
        board = {"profiles": {"drone_agent": {"tier": "lite"}}}
        self.assertEqual(emit.resolve_install_tier(board), "lite")

    def test_irrelevant_tier_returns_none(self) -> None:
        board = {"profiles": {"tier": "premium"}}
        self.assertIsNone(emit.resolve_install_tier(board))

    def test_no_profiles_block_returns_none(self) -> None:
        self.assertIsNone(emit.resolve_install_tier({}))


class BuildCurlInstallTests(unittest.TestCase):
    def test_drone_agent_lite_uses_install_lite_script(self) -> None:
        out = emit.build_curl_install("ados-drone-agent", "budget", "latest")
        self.assertEqual(out["method"], "curl")
        self.assertIn("install-lite.sh", out["command"])
        self.assertNotIn("--profile", out["command"])

    def test_drone_agent_lite_tier_alias_also_uses_lite_script(self) -> None:
        out = emit.build_curl_install("ados-drone-agent", "lite", "latest")
        self.assertIn("install-lite.sh", out["command"])

    def test_drone_agent_full_uses_install_script_without_profile_flag(self) -> None:
        out = emit.build_curl_install("ados-drone-agent", None, "latest")
        self.assertEqual(out["method"], "curl")
        # Must be the full installer, not the lite one.
        self.assertIn("install.sh", out["command"])
        self.assertNotIn("install-lite.sh", out["command"])
        self.assertNotIn("--profile", out["command"])

    def test_ground_agent_always_uses_profile_ground_station(self) -> None:
        for tier in (None, "budget", "lite", "premium", "standard"):
            with self.subTest(tier=tier):
                out = emit.build_curl_install(
                    "ados-ground-agent", tier, "latest",
                )
                self.assertEqual(out["method"], "curl")
                self.assertIn("--profile ground-station", out["command"])
                # Ground always installs the full agent, never the lite one.
                self.assertNotIn("install-lite.sh", out["command"])

    def test_latest_tag_uses_releases_latest_download_form(self) -> None:
        out = emit.build_curl_install("ados-drone-agent", None, "latest")
        self.assertIn("releases/latest/download/", out["command"])
        self.assertNotIn("raw.githubusercontent.com", out["command"])

    def test_concrete_release_tag_pins_specific_release(self) -> None:
        out = emit.build_curl_install(
            "ados-drone-agent", None, "lite-v0.1.4",
        )
        self.assertIn("releases/download/lite-v0.1.4/", out["command"])
        self.assertNotIn("releases/latest/", out["command"])

    def test_empty_release_tag_falls_back_to_latest(self) -> None:
        out = emit.build_curl_install("ados-drone-agent", None, "")
        self.assertIn("releases/latest/download/", out["command"])


class ProjectBoardTests(unittest.TestCase):
    def setUp(self) -> None:
        # Use a directory that's guaranteed to have no .img.gz artifacts so
        # the projection runs the no-image branch deterministically.
        self.empty_dist = REPO_ROOT / "tests" / "_emit_manifest_no_dist_fixture"

    def test_board_with_no_id_and_no_fallback_returns_none(self) -> None:
        # board.id missing AND fallback_id is empty string => no projection.
        result = emit.project_board(
            {"name": "Mystery"},
            version="0.0.0",
            dist_dir=self.empty_dist,
            fallback_id="",
        )
        self.assertIsNone(result)

    def test_board_uses_fallback_id_when_top_level_id_missing(self) -> None:
        result = emit.project_board(
            {
                "name": "Pi 4B",
                "soc": "BCM2711",
                "arch": "aarch64",
                "compute": {"ram_mb": 4096},
                "profiles": {},
            },
            version="0.0.0",
            dist_dir=self.empty_dist,
            fallback_id="rpi4b",
        )
        self.assertIsNotNone(result)
        assert result is not None
        self.assertEqual(result["id"], "rpi4b")
        self.assertIn("ados-ground-agent", result["stacks"])

    def test_web_flash_board_carries_bootrom_and_only_drone_stack(self) -> None:
        result = emit.project_board(
            {
                "board": {"id": "rv1106-g3"},
                "name": "Luckfox Pico Zero",
                "soc": "RV1106G3",
                "arch": "armv7l",
                "compute": {"ram_mb": 256},
                "profiles": {"tier": "budget"},
            },
            version="0.0.0",
            dist_dir=self.empty_dist,
            fallback_id="rv1106-g3",
            release_tag="latest",
        )
        self.assertIsNotNone(result)
        assert result is not None
        self.assertEqual(result["id"], "luckfox-pico-zero")
        self.assertEqual(result["stacks"], ["ados-drone-agent"])
        self.assertEqual(result["bootrom"]["vendorId"], 0x2207)
        install = result["installs"]["ados-drone-agent"]
        self.assertEqual(install["method"], "web-flash")
        # No image artifact in the empty dist dir => empty URL placeholder.
        self.assertEqual(install["imageUrl"], "")
        # Loader blob fields are omitted when no blob artifact exists.
        self.assertNotIn("loaderBlobUrl", install)
        self.assertNotIn("loaderBlobSha256", install)
        self.assertNotIn("loaderBlobMinisignSignature", install)


if __name__ == "__main__":
    unittest.main()
