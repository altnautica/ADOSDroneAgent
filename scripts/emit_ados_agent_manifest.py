"""Emit the ADOS Agent firmware manifest consumed by Mission Control.

The Mission Control Flash Tool fetches this JSON via its server-side proxy
and uses it to populate the board picker for the "ADOS Drone Agent" and
"ADOS Ground Agent" stacks. Each board declares per-stack install configs:

  - method "curl"      -> a single shell line the operator pastes onto a
                          board already running its stock vendor OS.
  - method "web-flash" -> URL + size + sha256 + minisign signature for an
                          .img.gz that the GCS flashes via WebUSB rockusb.

Run as:

    python scripts/emit_ados_agent_manifest.py [output_path]

Default output is dist/ados-agent-manifest.json. The local release flow
uploads this file as a GitHub Release asset on altnautica/ADOSDroneAgent
and the GCS proxy picks it up from the latest-release URL.

The script reads:
  - src/ados/hal/boards/*.yaml   for the SoC + RAM + arch board catalog
  - agents/lite-rs/Cargo.toml    for the agent version
  - dist/ados-<slug>-<version>*.img.gz   for image size + sha256
  - dist/ados-<slug>-<version>*.img.gz.minisig   for the signature

Boards without a published image artifact get an empty image-URL field;
the GCS displays a "no image published yet" notice for those entries
until the next release fills them in.
"""

from __future__ import annotations

import base64
import datetime
import hashlib
import json
import re
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
BOARDS_DIR = ROOT / "src" / "ados" / "hal" / "boards"
CARGO_TOML = ROOT / "agents" / "lite-rs" / "Cargo.toml"
DIST_DIR = ROOT / "dist"
DEFAULT_OUTPUT = DIST_DIR / "ados-agent-manifest.json"

LITE_INSTALL_CMD = (
    "curl -sSL https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/"
    "main/scripts/install-lite.sh | sudo bash"
)
FULL_INSTALL_CMD = (
    "curl -sSL https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/"
    "main/scripts/install.sh | sudo bash"
)
FULL_INSTALL_GROUND_CMD = (
    "curl -sSL https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/"
    "main/scripts/install.sh | sudo bash -s -- --profile ground-station"
)

# Boards the Flash Tool can write from the browser via Rockchip rockusb.
# Each entry maps a HAL board id to its bootrom USB IDs and the slug used
# by the imagebuilder recipe.
WEB_FLASH_BOARDS: dict[str, dict] = {
    "rv1106-g3": {
        "label": "Luckfox Pico Zero",
        "imagebuilder_slug": "luckfox-pico-zero",
        "bootrom": {"vendorId": 0x2207, "productId": 0x110C},
    },
}

# Minimum RAM (MB) for ground-agent eligibility when the HAL YAML doesn't
# carry an explicit ground_station block. Below this the WFB-ng + mesh
# stack is too tight; the board still qualifies as a drone agent.
GROUND_AGENT_MIN_RAM_MB = 2048

# HAL profile tiers that mean "Rust lite agent", not the Python full agent.
LITE_AGENT_TIERS = {"lite", "budget"}


def read_cargo_version(cargo_toml: Path) -> str:
    text = cargo_toml.read_text() if cargo_toml.exists() else ""
    match = re.search(r'^version\s*=\s*"([^"]+)"', text, flags=re.MULTILINE)
    return match.group(1) if match else "0.0.0"


def map_arch(hal_arch: str | None) -> str:
    """Map a HAL board's arch field to the GCS schema arch enum."""
    if not hal_arch:
        return "aarch64-glibc"
    arch = hal_arch.strip().lower()
    if arch in ("armv7l", "armv7", "arm"):
        return "armv7-musl"
    if arch in ("aarch64", "arm64"):
        return "aarch64-glibc"
    return "aarch64-glibc"


def resolve_stacks(board: dict) -> list[str]:
    """Decide which stacks a board can run.

    Drone agent runs on every supported SBC. Ground agent runs on boards
    where the HAL declares a `profiles.ground_station` sub-profile, OR
    on boards with >= 2 GB RAM as a conservative fallback when the HAL
    doesn't carry an explicit ground_station block yet.
    """
    profiles = board.get("profiles") or {}
    compute = board.get("compute") or {}

    stacks: list[str] = ["ados-drone-agent"]
    if "ground_station" in profiles:
        stacks.append("ados-ground-agent")
    elif (compute.get("ram_mb") or 0) >= GROUND_AGENT_MIN_RAM_MB:
        stacks.append("ados-ground-agent")
    return stacks


def resolve_install_tier(board: dict) -> str | None:
    """Return the lite/full agent tier for a board's drone-agent install."""
    profiles = board.get("profiles") or {}
    drone = profiles.get("drone_agent") or {}
    if isinstance(drone, dict) and drone.get("tier") in LITE_AGENT_TIERS:
        return drone["tier"]
    if profiles.get("tier") in LITE_AGENT_TIERS:
        return profiles["tier"]
    return None


def hash_artifact(path: Path) -> tuple[int, str]:
    data = path.read_bytes()
    return len(data), hashlib.sha256(data).hexdigest()


def read_minisig(sig_path: Path) -> str:
    """Return base64-encoded raw signature, dropping minisign comment lines."""
    if not sig_path.exists():
        return ""
    sig_lines = [
        line for line in sig_path.read_text().splitlines()
        if line and not line.startswith("untrusted") and not line.startswith("trusted")
    ]
    if not sig_lines:
        return ""
    raw = base64.b64decode(sig_lines[0])
    return base64.b64encode(raw).decode("ascii")


def build_curl_install(stack: str, tier: str | None) -> dict:
    is_lite = tier in LITE_AGENT_TIERS
    if stack == "ados-ground-agent":
        return {
            "method": "curl",
            "command": FULL_INSTALL_GROUND_CMD,
            "notes": [
                "Run on a board already booted into its vendor OS.",
                "Plug in your RTL8812EU adapter, OLED, and buttons before "
                "running so they auto-detect.",
            ],
        }
    return {
        "method": "curl",
        "command": LITE_INSTALL_CMD if is_lite else FULL_INSTALL_CMD,
        "notes": ["Run on a board already booted into its vendor OS."],
    }


def build_web_flash_install(image_artifact: Path | None) -> dict:
    notes = [
        "Hold the BOOT button while plugging USB-C into your computer to "
        "enter bootrom mode.",
        "Image flash erases the eMMC. Back up any user data first.",
    ]
    if image_artifact and image_artifact.exists():
        size, sha = hash_artifact(image_artifact)
        sig = read_minisig(
            image_artifact.with_suffix(image_artifact.suffix + ".minisig"),
        )
        return {
            "method": "web-flash",
            "imageUrl": (
                "https://github.com/altnautica/ADOSDroneAgent/releases/"
                f"latest/download/{image_artifact.name}"
            ),
            "sha256": sha,
            "minisignSignature": sig,
            "imageSizeBytes": size,
            "notes": notes,
        }
    return {
        "method": "web-flash",
        "imageUrl": "",
        "sha256": "",
        "minisignSignature": "",
        "imageSizeBytes": 0,
        "notes": notes,
    }


def find_image_artifact(slug: str, version: str, dist_dir: Path) -> Path | None:
    pattern = f"ados-{slug}-{version}*.img.gz"
    matches = sorted(dist_dir.glob(pattern))
    if matches:
        return matches[-1]
    fallback = sorted(dist_dir.glob(f"ados-{slug}-*.img.gz"))
    return fallback[-1] if fallback else None


def project_board(
    board: dict, version: str, dist_dir: Path, fallback_id: str,
) -> dict | None:
    board_meta = board.get("board") or {}
    board_id = board_meta.get("id") or fallback_id
    if not board_id:
        return None

    compute = board.get("compute") or {}
    profiles = board.get("profiles") or {}

    stacks = resolve_stacks(board)
    if not stacks:
        return None

    arch = map_arch(board.get("arch"))
    soc = board.get("soc") or "unknown"
    label = board.get("name") or board_id
    tier = resolve_install_tier(board)
    ram_mb = compute.get("ram_mb")

    description_parts: list[str] = []
    if ram_mb:
        description_parts.append(f"{ram_mb} MB RAM")
    storage = board.get("storage") or {}
    if storage.get("emmc"):
        description_parts.append("eMMC")
    elif storage.get("sd_card"):
        description_parts.append("microSD")
    description = ", ".join(description_parts) + "." if description_parts else None

    if board_id in WEB_FLASH_BOARDS:
        wf = WEB_FLASH_BOARDS[board_id]
        artifact = find_image_artifact(
            wf["imagebuilder_slug"], version, dist_dir,
        )
        installs = {
            "ados-drone-agent": build_web_flash_install(artifact),
        }
        result = {
            "id": wf["imagebuilder_slug"],
            "label": wf["label"],
            "soc": soc,
            "arch": arch,
            "stacks": ["ados-drone-agent"],
            "installs": installs,
            "bootrom": wf["bootrom"],
        }
        if description:
            result["description"] = description
        return result

    installs: dict[str, dict] = {}
    for stack in stacks:
        installs[stack] = build_curl_install(stack, tier)

    result = {
        "id": board_id,
        "label": label,
        "soc": soc,
        "arch": arch,
        "stacks": stacks,
        "installs": installs,
    }
    if description:
        result["description"] = description
    return result


def collect_boards(boards_dir: Path, version: str, dist_dir: Path) -> list[dict]:
    boards: list[dict] = []
    for path in sorted(boards_dir.glob("*.yaml")):
        with path.open() as fp:
            data = yaml.safe_load(fp)
        if not isinstance(data, dict):
            continue
        projected = project_board(data, version, dist_dir, fallback_id=path.stem)
        if projected:
            boards.append(projected)
    return boards


def main() -> int:
    output = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_OUTPUT
    version = read_cargo_version(CARGO_TOML)
    boards = collect_boards(BOARDS_DIR, version, DIST_DIR)
    boards.sort(key=lambda b: b["id"])

    manifest = {
        "schemaVersion": 1,
        "agentVersion": f"lite-v{version}",
        "generatedAt": datetime.datetime.now(datetime.timezone.utc)
        .replace(microsecond=0)
        .isoformat()
        .replace("+00:00", "Z"),
        "boards": boards,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("w") as fp:
        json.dump(manifest, fp, indent=2, sort_keys=False)
        fp.write("\n")
    print(f"Wrote {len(boards)} boards to {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
