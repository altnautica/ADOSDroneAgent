# DEPRECATED — superseded by `agents/lite-rs/imagebuilder/`

This directory was an early stab at a stock Buildroot tree (BR2_EXTERNAL
shape) for the lite-rs Rust drone agent. It assumed every target board
exposed a standard Buildroot defconfig that we could layer on. That
assumption does not hold:

- Vendor-SDK boards (Luckfox RV1103/1106 family) ship their own build
  orchestrator (`./build.sh lunch` → `allsave`) that downloads
  Buildroot dynamically and uses non-standard defconfig paths.
- Debian-rootfs boards (Raspberry Pi family) use pi-gen, not Buildroot.
- Rockchip 64-bit BSP boards (Radxa, Orange Pi) use Armbian or the
  Rockchip BSP, not stock Buildroot.

The new home is **`agents/lite-rs/imagebuilder/`** — a single
orchestrator with per-board recipes that speak each board's native
build language, plus a shared rootfs overlay and a single matrix CI
workflow.

## Migration map

| Old path | New path |
|---|---|
| `package/ados-rkmpi-wrapper/` | `imagebuilder/packaging/ados-rkmpi-wrapper/` (moved) |
| `board/luckfox_pico_zero/rootfs-overlay/etc/ados/ap-fallback/` | `imagebuilder/overlay/etc/ados/ap-fallback/` (moved; universal across boards) |
| `package/rtl8812eu/`, `package/aic8800/` | Driver cross-build done in-recipe via `recipe::build_drivers()`; `.ko` modules dropped into the rootfs overlay post-build. See `imagebuilder/boards/luckfox-pico-zero/drivers/` |
| `configs/luckfox_pico_zero_ados_defconfig` | `imagebuilder/boards/luckfox-pico-zero/patches/0001-add-our-packages-to-defconfig.patch` (applied against the SDK's `sysdrv/tools/board/buildroot/luckfox_pico_defconfig`) |
| `external.mk`, `external.desc`, `Config.in` | Not needed — orchestrator drives the SDK directly |

## What's left here

The old recipes (`package/rtl8812eu/`, `package/aic8800/`) are kept as
historical reference. They will be removed in the v0.2 cleanup wave once
no board recipe references them.

If a future board genuinely targets stock Buildroot (e.g. some Rockchip
SBC where we control the kernel + bootloader ourselves), this tree can
be revived. Until then it is read-only.
