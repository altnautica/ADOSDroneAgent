# Buildroot — Linux Image Building

This directory contains Buildroot configuration overlays for building production Linux images with ADOS ADOS Drone Agent pre-installed.

## Who Does What

| Role | Responsibility |
|------|---------------|
| **Chip vendor (Rockchip)** | Provides BSP (Board Support Package): kernel source, U-Boot, device tree templates |
| **Board designer (HGLRC)** | Customizes device tree for their PCB, selects peripherals, builds the image |
| **Software developer (Altnautica)** | Provides this overlay: pre-installed agent, systemd service, default config, first-boot scripts |

## Directory Structure

```
buildroot/
├── overlay/              # Files copied into the rootfs
│   ├── etc/ados/         # Default agent config
│   └── opt/ados/    # Pre-installed agent (venv + package)
└── configs/              # Per-chip Buildroot defconfig fragments
    ├── rv1126b_defconfig
    ├── rk3566_defconfig
    └── rk3506_defconfig
```

## How to Build (for HGLRC)

1. Clone the Rockchip Buildroot SDK for your chip
2. Copy `configs/<chip>_defconfig` into `buildroot/configs/`
3. Copy `overlay/` into `buildroot/board/<board>/rootfs_overlay/`
4. Run `make <chip>_defconfig && make`
5. Output: `output/images/sdcard.img`
6. Flash to eMMC via `rkdeveloptool` or Rockchip FactoryTool

## Overlay Contents

The overlay pre-installs ADOS ADOS Drone Agent so the image boots directly into operational mode:

- `/opt/ados/venv/` — Python virtual environment with all dependencies
- `/etc/ados/config.yaml` — Default configuration
- `/etc/systemd/system/ados.service` — Systemd service
- `/usr/local/bin/ados` — CLI symlink

## First Boot Behavior

1. Systemd starts `ados.service`
2. Agent detects no device ID exists
3. Generates UUID and saves to `/etc/ados/device-id`
4. Generates self-signed TLS certificate
5. Enables WiFi AP mode ("ADOS-XXXX")
6. Starts captive portal
7. User connects phone, completes setup wizard
8. Agent saves config, reboots into operational mode
