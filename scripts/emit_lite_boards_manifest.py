"""Emit the lite-eligible board manifest used by install.sh auto-detection.

Run as:

    python scripts/emit_lite_boards_manifest.py [output_path]

Default output is `dist/lite-boards.json` next to the repo root. The lite
agent CI release workflow uploads this file alongside the prebuilt
binaries; the install script downloads it before fingerprinting the SBC
to decide whether to install the Python full agent or the Rust lite
agent.

A board is lite-eligible when any of the following holds:

  - compute.ram_mb is missing or <= 384
  - profiles.tier in {"lite", "budget"}

The resulting JSON manifest contains only the fields install.sh needs
to fingerprint a running board:

  {
    "schema_version": 1,
    "boards": [
      {
        "id": "rv1106-g3",
        "name": "Luckfox Pico Zero RV1106G3",
        "arch": "armv7l",
        "ram_mb": 256,
        "model_patterns": ["rv1106-g3", "rv1106g3", "Luckfox Pico Zero"],
        "target_rust_triple": "armv7-unknown-linux-musleabihf",
        "init_system": "busybox",
        "libc": "uclibc"
      },
      ...
    ],
    "ram_failsafe_mb": 384
  }

The script is intentionally read-only and side-effect-free apart from
writing the output file. Designed to be called from CI without any
runtime dependencies beyond pyyaml.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
BOARDS_DIR = ROOT / "src" / "ados" / "hal" / "boards"
DEFAULT_OUTPUT = ROOT / "dist" / "lite-boards.json"

LITE_TIERS = {"lite", "budget"}
RAM_FAILSAFE_MB = 384


def is_lite_eligible(board: dict) -> bool:
    compute = board.get("compute") or {}
    ram = compute.get("ram_mb")
    if ram is not None and ram <= RAM_FAILSAFE_MB:
        return True
    profiles = board.get("profiles") or {}
    if profiles.get("tier") in LITE_TIERS:
        return True
    return False


def project(board: dict) -> dict:
    """Pick only the fields install.sh actually uses for fingerprinting."""
    board_meta = board.get("board") or {}
    compute = board.get("compute") or {}
    return {
        "id": board_meta.get("id"),
        "name": board.get("name"),
        "arch": board.get("arch"),
        "ram_mb": compute.get("ram_mb"),
        "model_patterns": board.get("model_patterns") or [],
        "target_rust_triple": board.get("target_rust_triple"),
        "init_system": board.get("init_system"),
        "libc": board.get("libc"),
    }


def collect_boards(boards_dir: Path) -> list[dict]:
    boards: list[dict] = []
    for path in sorted(boards_dir.glob("*.yaml")):
        with path.open() as fp:
            data = yaml.safe_load(fp)
        if not isinstance(data, dict):
            continue
        if not is_lite_eligible(data):
            continue
        projected = project(data)
        if not projected.get("id"):
            # board.id is required for install.sh dispatch; skip malformed
            continue
        boards.append(projected)
    return boards


def main() -> int:
    output = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_OUTPUT
    boards = collect_boards(BOARDS_DIR)
    manifest = {
        "schema_version": 1,
        "ram_failsafe_mb": RAM_FAILSAFE_MB,
        "boards": boards,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("w") as fp:
        json.dump(manifest, fp, indent=2, sort_keys=False)
        fp.write("\n")
    print(f"Wrote {len(boards)} lite-eligible boards to {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
