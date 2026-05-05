# HAL Board YAML — Lite-RS Schema Extension

Documents the additive fields the lite Rust agent reads from `src/ados/hal/boards/*.yaml`, on top of the base schema in `src/ados/hal/detect.py`.

## Decision

The lite agent reuses the existing HAL board YAML registry as the canonical source of board metadata. The eight additional fields below are additive only — every existing board YAML continues to load unchanged, and consumers that don't read these fields are unaffected.

The Pydantic `BoardProfile` model uses default `extra="ignore"` configuration, so unknown YAML fields are silently dropped during validation. The Rust agent reads the same YAML files via `serde_yaml` and applies sensible defaults when fields are absent.

## Additive fields

### Top-level

| Field | Type | Default | Purpose |
|---|---|---|---|
| `libc` | `"glibc" \| "uclibc" \| "musl"` | `"glibc"` | Userspace C library flavor on the rootfs |
| `init_system` | `"systemd" \| "busybox" \| "runit" \| "s6"` | `"systemd"` | Init system the agent ships an integration unit for |
| `target_rust_triple` | string | inferred from `arch` | Rust cross-compile target triple for this board |
| `min_kernel_version` | string | none | Minimum Linux kernel version, e.g. `"5.10"` |
| `wifi_chip_driver` | string | none | Kernel module name for the Wi-Fi chip's driver, when relevant for out-of-tree drivers |

### Inside the `video` block

| Field | Type | Default | Purpose |
|---|---|---|---|
| `video.encoder_api_lite` | `"v4l2" \| "rkmpi" \| "libcamera" \| "none"` | `"none"` | Encoder API the lite agent dispatches video through |
| `video.vendor_lib_loader` | `"none" \| "rkmpi-subprocess" \| "dlopen-vendorlib"` | `"none"` | Strategy for loading vendor video libraries when present |

### Inside the `compute` block

| Field | Type | Default | Purpose |
|---|---|---|---|
| `compute.min_ram_mb` | int | none | Absolute minimum RAM (in MB) required to boot the lite agent on this board |

## Example — Luckfox Pico Zero (constrained ARMv7 board)

```yaml
# excerpt from src/ados/hal/boards/rv1106-g3.yaml after extension
libc: "uclibc"
init_system: "busybox"
target_rust_triple: "armv7-unknown-linux-musleabihf"
min_kernel_version: "5.10"
wifi_chip_driver: "aic8800-dkms"

compute:
  cores: 1
  core_type: "Cortex-A7 @ 1.2 GHz"
  ram_mb: 256
  min_ram_mb: 96

video:
  encoder_api: "rkmpi"
  encoder_api_lite: "rkmpi"
  vendor_lib_loader: "rkmpi-subprocess"
```

## Example — Pi Zero 2 W (moderately-constrained ARMv8 board)

```yaml
# excerpt from src/ados/hal/boards/pi-zero-2w.yaml
libc: "glibc"
init_system: "systemd"
target_rust_triple: "aarch64-unknown-linux-gnu"
min_kernel_version: "5.10"
wifi_chip_driver: "brcmfmac"

compute:
  cores: 4
  core_type: "Cortex-A53 @ 1.0 GHz"
  ram_mb: 512
  min_ram_mb: 96

video:
  encoder_api: "none"
  encoder_api_lite: "libcamera"
  vendor_lib_loader: "none"
```

## Detection and dispatch

At startup the lite agent:

1. Reads the board override file at `/etc/ados/board_override` if present (a single-line board id matching one of the YAMLs).
2. Otherwise reads `/proc/device-tree/model` and matches against each YAML's `model_patterns` list.
3. Otherwise falls back to `/proc/cpuinfo` Hardware/model substring matching.
4. Loads the matched YAML at `/opt/ados/hal/boards/<id>.yaml` (or the in-source path during development).
5. Dispatches video / Wi-Fi / init concerns through the board-specific fields above.

When a field is absent the documented default applies. A board YAML with no `init_system` still loads cleanly and the lite agent treats it as a systemd target (the most common case).

## Conformance notes

- New fields must remain optional with sensible defaults. A required-without-default field would break every existing YAML at parse.
- Field names within nested blocks (`video.*`, `compute.*`) sit alongside existing keys in those blocks (`encoder_api`, `ram_mb`, etc.). Don't rename existing keys.
- Future extensions should follow the same pattern: additive only, optional, defaulted.
