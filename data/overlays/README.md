# Device-tree overlays for SPI LCDs

Per-board overlays that bind the Waveshare 3.5" RPi LCD (A) — ILI9486
SPI display + ADS7846 / XPT2046 resistive touch — to different
single-board computers.

## Layout

```
data/overlays/
├── README.md                                       ← this file
├── upstream/
│   ├── waveshare35-lcd.dtsi                        ← shared chip-level dtsi
│   ├── rk3588-spi4-m2-cs0-waveshare35.dts          ← Rock 5C/5A/5D SPI LCD + touch
│   └── rk3588-spi4-m2-cs1-xpt2046-touch.dts        ← Rock 5C/5A/5D touch-only (HDMI video)
└── cubie-a7z-waveshare35a.dts                      ← Radxa Cubie A7Z (Allwinner A733)
```

The `rk3588-spi4-m2-cs1-xpt2046-touch.dts` overlay is the panel-stripped
sibling of the Rock 5C SPI-LCD overlay: an `ads7846` touch node on CS1 with
no ILI9486 framebuffer, for an HDMI display that carries a standalone XPT2046
resistive-touch layer (video over HDMI, touch over SPI). It is a standalone
overlay (it does not `#include` the shared dtsi, which declares the panel),
so it inlines the chip-level touch properties itself and the invariant lint
skips it with a WARN by design. It keeps the same known-good `GPIO_ACTIVE_LOW`
PENIRQ polarity + `EDGE_FALLING` trigger. See
`scripts/drivers/install-display-overlay.sh` (the `hdmi-touch` provisioning
path) for how it is compiled + activated and how the
`LIBINPUT_CALIBRATION_MATRIX` udev rule is derived from the board's declared
touch bounds.

## The chip-vs-SoC contract

Every overlay describes the **same touch + display silicon** wired
onto a **different SoC and pinout**. The chip-level facts — the
ILI9486 display controller mode, the ADS7846 touch chip's pressure
range, its plate resistance, the open-drain PENIRQ polarity — never
change. The SoC-level facts — which GPIO bank holds the pendown line,
which pinctrl group sets the pull-up bias, which SPI controller hosts
the panel — change every time we add a new board.

We learned the hard way that letting per-board overlays redeclare
chip-level facts produces drift:

- Cubie A7Z silently omitted `ti,invert-y` so its panel ran with Y
  unflipped while Rock 5C inherited `ti,invert-y = <1>`.
- Rock 5C shipped `pendown-gpio = GPIO_ACTIVE_HIGH` for months. Every
  Pi-canonical overlay uses ACTIVE_LOW because PENIRQ is open-drain.
  The polarity was copy-pasted from a Radxa Rock 3B SPI-LCD adaptation
  template that itself used a different touch chip. Touch was dead
  across boots until v0.28.22.
- Both per-board overlays carried their own `touchscreen-max-pressure`
  / `ti,pressure-max` lines, redundant with the shared dtsi.

So the contract is now mechanical: **the shared dtsi is the single
source of truth for chip-level invariants, and per-board overlays
override only SoC-specific fields.** The lint script at
`scripts/test/lint_overlay_invariants.sh` enforces this in CI.

### Banned in per-board overlays (live in shared dtsi)

| Field | Why |
|---|---|
| `compatible` (of ads7846@1 or ili9486@0) | Chip identity, invariant |
| `reg` (of ads7846@1 or ili9486@0) | SPI CS slot is invariant (touch on CS1, display on CS0) |
| `spi-max-frequency` | Datasheet upper bound, same per chip family |
| `ti,x-plate-ohms` | Plate resistance of this panel, same on every board |
| `ti,pressure-max` | Full-scale pressure reading, same on every board |
| `ti,swap-xy` | Axis swap; same on every board |
| `ti,invert-y` | Y polarity for this panel's resistive layer; same on every board |
| `touchscreen-max-pressure` | Modern binding name for the same full-scale value |

The lint refuses to merge a per-board overlay that redeclares any of
these. If a future panel needs a different value, add a NEW shared
dtsi for that panel — do not override per-board.

### Allowed in per-board overlays (genuinely SoC-specific)

| Field | Why |
|---|---|
| `interrupt-parent` | GPIO bank phandle, SoC-specific |
| `interrupts` | Pin index inside the bank + IRQ trigger; bank index is SoC-specific |
| `pendown-gpio` | Same bank+pin reference as `interrupts` |
| `pinctrl-names` + `pinctrl-0` | Pinctrl mux group label, SoC-specific |
| `vcc-supply` | Regulator phandle name, SoC-specific (`&vcc5v0_sys` on Rockchip, `&reg_vcc5v` on Allwinner, etc.) |
| `reset-gpios` + `dc-gpios` | GPIO bank+pin for ILI9486 control lines |
| `cs-gpios` + `num-cs` | SPI CS routing |
| Pinctrl group nodes under `&pio` / `&pinctrl` / `&r_pio` | Pinmux + bias config, SoC-specific |

### Polarity — ALWAYS `GPIO_ACTIVE_LOW`

The XPT2046 / ADS7846 PENIRQ output is open-drain with a chip-side
internal pull-up (~50 kΩ). The pin idles HIGH at VCC and drops to LOW
when a finger touches the panel. In gpiolib semantics, that's
**`GPIO_ACTIVE_LOW`** — the logical "asserted" state corresponds to
the electrical LOW level. The IRQ trigger stays
`IRQ_TYPE_EDGE_FALLING` because the controller sees the physical
HIGH→LOW transition on touch contact.

```c
&ads7846 {
    interrupts = <PIN_INDEX IRQ_TYPE_EDGE_FALLING>;
    pendown-gpio = <&gpioN PIN_INDEX GPIO_ACTIVE_LOW>;
};
```

Every working Pi-canonical Waveshare overlay
([swkim01/waveshare-dtoverlays](https://github.com/swkim01/waveshare-dtoverlays),
[goodtft/LCD-show](https://github.com/goodtft/LCD-show),
[marcin-chwedczuk/waveshare-35A-raspberry-pi-64-driver](https://github.com/marcin-chwedczuk/waveshare-35A-raspberry-pi-64-driver))
ships ACTIVE_LOW. The Cubie A7Z overlay ships ACTIVE_LOW. **Do not
copy ACTIVE_HIGH from any Rockchip vendor reference**; the templates
floating around for Rock 3B and other boards came from a chip that
wasn't ADS7846.

### Pull-up bias — pinctrl group needed on RK3588

The Rock 5C overlay declares a pinctrl group that puts the PENIRQ pin
into GPIO function with `pcfg_pull_up`. Without it the kernel reports
`(MUX UNCLAIMED)` for the pin and the line floats — `/proc/interrupts
ads7846` stays at 0 across boots. The Cubie A7Z overlay does the same
via Allwinner's `bias-pull-up` property. **Every new per-board
overlay must include a pinctrl-0 reference for the PENIRQ pin and
set a pull-up bias.**

## Adding a new board

1. Pick the closest existing overlay as a template:
   - Allwinner/sunxi → start from `cubie-a7z-waveshare35a.dts`.
   - Rockchip → start from `upstream/rk3588-spi4-m2-cs0-waveshare35.dts`.
   - New SoC family → write a new overlay from scratch, but keep the
     same `#include` of the shared dtsi.
2. `#define DISPLAY_SPI <controller>` (your board's SPI controller phandle).
3. `#include "upstream/waveshare35-lcd.dtsi"`.
4. Add only the SoC-specific overrides (`&DISPLAY_SPI`, `&ili9486`,
   `&ads7846`, plus pinctrl group nodes under `&pio` / `&pinctrl` /
   `&r_pio`).
5. Set `pendown-gpio` polarity to `GPIO_ACTIVE_LOW`. Set IRQ trigger
   to `IRQ_TYPE_EDGE_FALLING`.
6. Add a pinctrl group that puts the PENIRQ pin into GPIO function
   with a pull-up bias. Reference it from `&ads7846 { pinctrl-0 = ... }`.
7. Run locally before committing:
   ```bash
   scripts/test/lint_overlay_invariants.sh
   scripts/test/compile_overlays.sh
   ```
   Both must exit 0. The compile script no-ops on macOS (no kernel
   headers); use a Linux box or the CI run for that pass.
8. Add board detection in
   `scripts/drivers/install-display-overlay.sh:129-168` so the
   installer picks the new overlay automatically.

## Validating on a real board

After installing the agent:

```bash
ados status                                          # Setup % bumps when display.conf is written
cat /etc/ados/display.conf                           # framebuffer_path matches /sys/class/graphics/fb*/name
ls /sys/class/graphics/                              # fb_ili9486 entry appears
cat /proc/interrupts | grep ads7846                  # counter increments per tap
```

Decompile the live dtbo to confirm the overlay applied with the
expected polarity:

```bash
sudo dtc -I dtb -O dts /boot/dtbo/<your-overlay>.dtbo 2>/dev/null \
    | grep -E 'pendown-gpio|interrupts |interrupt-parent'
```

`pendown-gpio` should end with `0x01` (ACTIVE_LOW). `interrupts`
trigger cell should be `0x02` (IRQ_TYPE_EDGE_FALLING).

## References

- ADS7846 binding documentation: `Documentation/devicetree/bindings/input/touchscreen/ads7846.txt`
- XPT2046 datasheet: open-drain PENIRQ behavior + pull-up requirement
- Waveshare 3.5" RPi LCD (A) wiki: pin mapping on the 40-pin header
- Pi-canonical overlay: `swkim01/waveshare-dtoverlays/waveshare35a.dts`
