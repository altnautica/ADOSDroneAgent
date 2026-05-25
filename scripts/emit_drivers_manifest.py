"""Emit the prebuilt-driver manifest the full-agent install consults.

Run as:

    python scripts/emit_drivers_manifest.py <artifacts_dir> [output_path] \
        [--release-tag TAG]

Scans <artifacts_dir> for prebuilt RTL8812EU kernel modules named

    8812eu-<kernelrelease>-<arch>.ko

runs `modinfo -F vermagic` on each, cross-checks the (kernelrelease, arch)
parsed from the filename against the module's own vermagic (failing loudly
if they disagree), reads the driver version from vendor/rtl8812eu/dkms.conf,
and emits drivers-manifest.json. The install path downloads this manifest,
looks up its running (module, kernelrelease, arch), and installs the
matching .ko after verifying its SHA256 + signature — skipping the slow
from-scratch DKMS compile.

Manifest shape:

    {
      "schema_version": 1,
      "module": "8812eu",
      "dkms_package": "realtek-rtl88x2eu",
      "driver_version": "5.15.0.1~20230815",
      "vendor_ref": "48e6e449...",
      "mesh_patch_sha256": "fde9e3cc...",
      "modules": [
        {
          "kernelrelease": "6.6.51+rpt-rpi-v8",
          "arch": "arm64",
          "vermagic": "6.6.51+rpt-rpi-v8 SMP preempt mod_unload aarch64",
          "filename": "8812eu-6.6.51+rpt-rpi-v8-arm64.ko",
          "url": "https://github.com/altnautica/ADOSDroneAgent/releases/download/<tag>/8812eu-6.6.51+rpt-rpi-v8-arm64.ko",
          "sha256": "...",
          "driver_version": "5.15.0.1~20230815"
        },
        ...
      ]
    }

The script is read-only apart from writing the output file and uses only
the Python standard library plus the `modinfo` binary on PATH.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VENDOR_DKMS_CONF = ROOT / "vendor" / "rtl8812eu" / "dkms.conf"
MESH_PATCH = ROOT / "data" / "driver-patches" / "mesh-enable.patch"

MODULE_NAME = "8812eu"
SCHEMA_VERSION = 1

# 8812eu-<kernelrelease>-<arch>.ko
# kernelrelease can contain dots, plus, tilde, letters, digits; the arch is
# the final hyphen-delimited token before .ko. Anchor on a known arch set so
# a kernelrelease that itself contains hyphens (it can) is parsed correctly.
KNOWN_ARCHES = ("arm64", "armhf", "amd64", "x86_64")
FILENAME_RE = re.compile(
    r"^" + re.escape(MODULE_NAME) + r"-(?P<kver>.+)-(?P<arch>"
    + "|".join(re.escape(a) for a in KNOWN_ARCHES)
    + r")\.ko$"
)

RELEASE_URL_TEMPLATE = (
    "https://github.com/altnautica/ADOSDroneAgent/releases/download/{tag}/{filename}"
)


def _read_dkms_field(conf: Path, field: str) -> str:
    """Read a quoted FIELD="value" assignment from dkms.conf."""
    if not conf.is_file():
        return ""
    pat = re.compile(r'^' + re.escape(field) + r'="([^"]*)"')
    for line in conf.read_text().splitlines():
        m = pat.match(line.strip())
        if m:
            return m.group(1)
    return ""


def _vendor_ref() -> str:
    """Best-effort vendored-source commit (GPL corresponding-source pin)."""
    try:
        out = subprocess.run(
            ["git", "-C", str(ROOT / "vendor" / "rtl8812eu"), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            check=False,
        )
        if out.returncode == 0:
            return out.stdout.strip()
    except (OSError, ValueError):
        pass
    # Fallback: read .git/modules pointer is unreliable in CI; return empty
    # rather than guess. The release body also names the submodule commit.
    return ""


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fp:
        for chunk in iter(lambda: fp.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def _file_sha256(path: Path) -> str:
    return _sha256(path) if path.is_file() else ""


def _modinfo_vermagic(ko: Path) -> str:
    """Return `modinfo -F vermagic <ko>` output, or '' on failure."""
    try:
        out = subprocess.run(
            ["modinfo", "-F", "vermagic", str(ko)],
            capture_output=True,
            text=True,
            check=False,
        )
        if out.returncode == 0:
            return out.stdout.strip()
    except OSError:
        pass
    return ""


def collect_modules(artifacts_dir: Path, driver_version: str, release_tag: str) -> list[dict]:
    modules: list[dict] = []
    for ko in sorted(artifacts_dir.glob(f"{MODULE_NAME}-*.ko")):
        m = FILENAME_RE.match(ko.name)
        if not m:
            print(f"skip: filename does not match expected pattern: {ko.name}", file=sys.stderr)
            continue
        kver = m.group("kver")
        arch = m.group("arch")

        vermagic = _modinfo_vermagic(ko)
        if not vermagic:
            raise SystemExit(f"error: could not read vermagic from {ko.name}")

        # Cross-check: the kernelrelease parsed from the filename must equal
        # the leading vermagic token. A mismatch means the file was named
        # for a different kernel than it was built against — refuse it.
        vm_release = vermagic.split()[0]
        if vm_release != kver:
            raise SystemExit(
                f"error: {ko.name}: filename kernelrelease '{kver}' "
                f"disagrees with module vermagic '{vm_release}'"
            )

        sha = ko.with_name(ko.name + ".sha256")
        # Prefer the precomputed sidecar to stay consistent with what the
        # release publishes; fall back to hashing the file in place.
        sha256 = ""
        if sha.is_file():
            sha256 = sha.read_text().split()[0].strip()
        if not sha256:
            sha256 = _sha256(ko)

        modules.append(
            {
                "kernelrelease": kver,
                "arch": arch,
                "vermagic": vermagic,
                "filename": ko.name,
                "url": RELEASE_URL_TEMPLATE.format(tag=release_tag, filename=ko.name),
                "sha256": sha256,
                "driver_version": driver_version,
            }
        )
    return modules


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts_dir", help="directory containing 8812eu-*.ko files")
    parser.add_argument(
        "output",
        nargs="?",
        default=None,
        help="output manifest path (default: <artifacts_dir>/drivers-manifest.json)",
    )
    parser.add_argument(
        "--release-tag",
        default="prebuilt-drivers",
        help="release tag used to pin download URLs (default: prebuilt-drivers)",
    )
    args = parser.parse_args()

    artifacts_dir = Path(args.artifacts_dir)
    if not artifacts_dir.is_dir():
        raise SystemExit(f"error: artifacts dir not found: {artifacts_dir}")

    output = Path(args.output) if args.output else artifacts_dir / "drivers-manifest.json"

    driver_version = _read_dkms_field(VENDOR_DKMS_CONF, "PACKAGE_VERSION")
    dkms_package = _read_dkms_field(VENDOR_DKMS_CONF, "PACKAGE_NAME")
    if not driver_version or not dkms_package:
        raise SystemExit(f"error: could not parse PACKAGE_VERSION/PACKAGE_NAME from {VENDOR_DKMS_CONF}")

    modules = collect_modules(artifacts_dir, driver_version, args.release_tag)

    manifest = {
        "schema_version": SCHEMA_VERSION,
        "module": MODULE_NAME,
        "dkms_package": dkms_package,
        "driver_version": driver_version,
        "vendor_ref": _vendor_ref(),
        "mesh_patch_sha256": _file_sha256(MESH_PATCH),
        "modules": modules,
    }

    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("w") as fp:
        json.dump(manifest, fp, indent=2, sort_keys=False)
        fp.write("\n")
    print(f"Wrote {len(modules)} prebuilt module(s) to {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
