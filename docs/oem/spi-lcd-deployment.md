# SPI LCD on Ground-Station Companion Boards

This guide covers attaching a 3.5" SPI LCD (ILI9486 + ADS7846 resistive
touch, also known as the "RPi LCD A" form factor) to two supported
companion boards: the Radxa Cubie A7Z (Allwinner A733) and the Radxa
Rock 5C / Rock 5C Lite (Rockchip RK3582 / RK3588S2).

After running the agent install one-liner the LCD lights up on the
next boot showing the same role / mesh / peer status as the legacy
128x64 OLED bench rig, scaled and centered on the 480x320 panel. A
single tap advances the screen cycle.

## What you need

* Ground-station companion board with a free 40-pin expansion header
  (Cubie A7Z or Rock 5C / Rock 5C Lite).
* Waveshare 3.5" RPi LCD (A) or another panel that drives the same
  ILI9486 + ADS7846 controller pair over SPI.
* USB-C power supply rated for the board's max draw.
* Network access for the install (Wi-Fi or Ethernet) so the script
  can pull packages.

The HAT plugs onto the 40-pin header without rewiring on either
board. Power, ground, SPI clock / MOSI / MISO / CS, the LCD reset and
DC pins, and the touch IRQ all land on Pi-pinout-compatible positions.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install.sh \
    | sudo bash -s -- --pair YOUR_PAIRING_CODE --profile ground-station
```

The script:

1. Detects the board via `/proc/device-tree/model`.
2. Reads the board's YAML profile to find which display ids the board
   supports. For Cubie A7Z and Rock 5C the default supported display
   is `waveshare35a`.
3. Provisions the device-tree overlay:
    * Cubie A7Z: compiles `data/overlays/cubie-a7z-waveshare35a.dts`
      via `dtc` and drops the DTBO at
      `/boot/overlay-user/cubie-a7z-waveshare35a.dtbo`. Activates by
      appending an `fdtoverlays` line to whichever boot config the
      BSP uses (`/boot/extlinux/extlinux.conf`,
      `/boot/orangepiEnv.txt`, or `/boot/armbianEnv.txt`).
    * Rock 5C: activates the BSP-shipped DTBO at
      `/boot/dtbo/rk3588-spi4-m2-cs0-waveshare35.dtbo` by appending
      its name to `/boot/dtb/rockchip/overlays-list` (Radxa OS) or
      running `update-u-boot` (Armbian). When the BSP overlay package
      is absent on a third-party Rockchip image, the script falls
      back to compiling a vendored copy of the source.
4. Writes `/etc/modules-load.d/ados-display.conf` so `fbtft`,
   `fb_ili9486`, and `ads7846` load on every boot.
5. Writes `/etc/ados/display.conf` with the resolved display id,
   framebuffer path, expected driver name, touch state, rotation,
   and which boot-config file was edited (so the next operator can
   trace the change).

## Reboot

```sh
sudo reboot
```

After the reboot:

* `ls /dev/fb*` shows `fb0` and `fb1`.
* `cat /sys/class/graphics/fb1/name` reports `fb_ili9486`.
* The LCD displays the ADOS status carousel, cycling automatically
  every 5 seconds across role, mesh, peers, and uplink.
* Touching anywhere on the panel advances to the next screen
  immediately. Debounce is 250 ms.

## Choosing a different display id, or skipping

`ADOS_DISPLAY` controls the install-time selection:

```sh
# Force a specific display id even if board YAML says otherwise:
sudo ADOS_DISPLAY=waveshare35a ./install.sh ...

# Skip LCD provisioning entirely (e.g. on a board where you want to
# attach the LCD later):
sudo ADOS_DISPLAY=none ./install.sh ...
```

Re-running `install.sh --upgrade` later re-runs the LCD-overlay step
on ground-station profiles, so an operator who plugs the LCD onto an
already-installed board just runs the upgrade once and reboots.

## Verify state

The agent's hardware-check API surfaces the display state:

```sh
curl -fsSL http://<board>:8080/v1/setup/hardware-check
```

Look for the `display` row. It reports one of:

| State | Meaning | Fix |
|---|---|---|
| `ok` | configured + framebuffer bound + driver matches expected | nothing |
| `pending_reboot` | configured but `/dev/fb1` not bound yet | `sudo reboot` |
| `unknown` (not_configured) | `/etc/ados/display.conf` missing | rerun install with `--upgrade` after attaching the LCD |
| `warning` (driver mismatch) | `/dev/fb1` bound, but the driver name in `/sys/class/graphics/fb1/name` does not contain the expected token | rerun install with `--upgrade` to refresh the overlay, then reboot |

The cloud heartbeat carries the same information up to Mission
Control as a `peripherals[]` entry with `category="display"`. Mission
Control renders an `LCD` pill on the drone card and shows the panel
details under `Hardware → Peripherals → Local Display`.

## Manual overlay compilation (debugging)

When the install step fails, you can compile the overlay by hand to
isolate the problem.

Cubie A7Z:

```sh
sudo apt-get install -y device-tree-compiler
dtc -@ -I dts -O dtb \
    -o /tmp/cubie-a7z-waveshare35a.dtbo \
    /opt/ados/source/data/overlays/cubie-a7z-waveshare35a.dts
sudo install -m 0644 /tmp/cubie-a7z-waveshare35a.dtbo \
    /boot/overlay-user/cubie-a7z-waveshare35a.dtbo
sudo reboot
```

Rock 5C, when the BSP package is absent:

```sh
sudo apt-get install -y device-tree-compiler linux-headers-$(uname -r)
KBUILD=/lib/modules/$(uname -r)/build/include
cpp -E -x assembler-with-cpp -undef -nostdinc \
    -I "$KBUILD" -I /usr/include \
    /opt/ados/source/data/overlays/upstream/rk3588-spi4-m2-cs0-waveshare35.dts \
    -o /tmp/rk3588-spi4.preprocessed.dts
dtc -@ -I dts -O dtb \
    -o /tmp/rk3588-spi4-m2-cs0-waveshare35.dtbo \
    /tmp/rk3588-spi4.preprocessed.dts
sudo install -m 0644 /tmp/rk3588-spi4-m2-cs0-waveshare35.dtbo \
    /boot/dtbo/rk3588-spi4-m2-cs0-waveshare35.dtbo
echo rk3588-spi4-m2-cs0-waveshare35 | sudo tee -a /boot/dtb/rockchip/overlays-list
sudo reboot
```

## Troubleshooting

| Symptom | Likely cause | First check |
|---|---|---|
| LCD stays dark after reboot | overlay never loaded | `dmesg | grep -iE 'fb_ili9486|ads7846|spi'` |
| Display shows random colored snow | wrong rotation or BGR setting | adjust `rotate` and `bgr` overlay parameters |
| Touch reports the wrong corner | swap-xy / invert-x / invert-y wrong for your panel | tune `ti,swap-xy`, `ti,invert-x`, `ti,invert-y` |
| Touch fires while idle | PENIRQ pin held low (likely a bad solder joint or short to ground) | check the IRQ trace on the HAT and the SoC pin |
| `journalctl -u ados-oled` says `framebuffer renderer attached` but nothing on screen | mmap succeeded but display is unpowered | check 5V rail at the HAT and the LCD's backlight rail |

## Hardware reference

The 40-pin header signal positions used by both supported boards:

| Pi physical pin | Cubie A7Z | Rock 5C |
|---|---|---|
| 19 | PD12 / SPI1 MOSI | GPIO1_A1 / SPI4_MOSI_M2 |
| 21 | PD13 / SPI1 MISO | GPIO1_A0 / SPI4_MISO_M2 |
| 23 | PD11 / SPI1 CLK | GPIO1_A2 / SPI4_CLK_M2 |
| 24 | PD10 / SPI1 CS0 (LCD) | GPIO1_A3 / SPI4_CS0_M2 (LCD) |
| 26 | PD14 / GPIO CS1 (touch) | GPIO1_A4 / GPIO CS1 (touch) |
| 18 | PJ25 / GPIO out (LCD DC) | GPIO1_B0 / GPIO out (LCD DC) |
| 22 | PL5 (AO domain) / GPIO out (LCD RST) | GPIO1_B5 / GPIO out (LCD RST) |
| 11 | PB1 / GPIO in (touch IRQ) | GPIO4_B3 / GPIO in (touch IRQ) |

Power on pins 1, 17 (3V3) and 2, 4 (5V). Ground on pins 6, 9, 14, 20,
25, 30, 34, 39.

The LCD's IRQ pin is wired to the ADS7846 PENIRQ output, which is
open-drain pull-low when the pen is down. Both overlays declare the
IRQ as falling-edge triggered and the pendown-gpio as ACTIVE_LOW.
