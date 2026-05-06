# Luckfox Pico Zero — flash guide (ADOS lite agent)

This guide covers flashing the official ADOS image onto a Luckfox Pico Zero
microSD and bringing the device up to a paired state in under one minute.

The image is the canonical OEM integration path: an operator flashes one
file, applies power, reads a code off the UART or display, and types it
into Mission Control. No internet access is required at the SBC during
install — the curl-pipe install path remains available and unchanged for
operators who prefer it.

## What you need

- Luckfox Pico Zero (RV1106G3, 256 MB RAM)
- microSD card, 4 GB or larger, class 10 or better
- USB-UART cable hooked to the board's debug pins (3.3 V; pinout in the
  board's reference docs) — recommended for first-boot pair-code readout
- A serial terminal (115200 8N1) — `screen`, `minicom`, `picocom`, or PuTTY

## Download the image

Latest stable images are published to GitHub Releases under tags matching
`lite-image-v*`:

```
https://github.com/altnautica/ADOSDroneAgent/releases
```

The release carries these artifacts per version:

| File | Purpose |
|---|---|
| `ados-luckfox-pico-zero-vX.Y.Z.img.gz` | Compressed flashable image |
| `ados-luckfox-pico-zero-vX.Y.Z.img.gz.minisig` | Per-file Ed25519 signature (minisign) |
| `ados-luckfox-pico-zero-vX.Y.Z.img.gz.sha256` | Per-file SHA256 checksum |
| `SHA256SUMS` | Aggregated checksum manifest covering every artifact in the release |
| `SHA256SUMS.minisig` | Ed25519 signature over `SHA256SUMS` |

Download `SHA256SUMS`, `SHA256SUMS.minisig`, and the matching image
file to the same directory.

## Verify the image

Always verify before flashing. The minisign Ed25519 signature is the
strong check; SHA256 is a fast integrity check.

Install `minisign` from your package manager (Debian/Ubuntu:
`apt-get install minisign`; Alpine: `apk add minisign`; macOS:
`brew install minisign`). Upstream source and docs:
[`jedisct1/minisign`](https://github.com/jedisct1/minisign).

Confirm the public key the installer trusts:

```sh
./scripts/install-lite.sh --show-key
```

The fingerprint printed should match the fingerprint listed at the top
of the GitHub Release notes for the version you are flashing. See
`docs/oem/key-rotation-policy.md` for what to do if it doesn't.

Verify the SHA256 list against the public key's signature, then verify
the image against the SHA256 list:

```sh
# Verify the SHA256 list itself was signed by the trusted key
minisign -Vm SHA256SUMS -P "$(./scripts/install-lite.sh --show-key | awk '/public key/ {print $3}')"

# Verify the image bytes match the signed SHA256 entry
sha256sum -c SHA256SUMS
```

Per-artifact `.minisig` files are also published for operators who
prefer signing each file directly:

```sh
minisign -V \
  -P "$(./scripts/install-lite.sh --show-key | awk '/public key/ {print $3}')" \
  -m ados-luckfox-pico-zero-vX.Y.Z.img.gz \
  -x ados-luckfox-pico-zero-vX.Y.Z.img.gz.minisig
```

`Signature and comment signature verified` confirms a clean candidate.
Anything else means the file is tampered or the wrong key was supplied.

If either check fails, do not flash — re-download.

## Flash the image

### macOS / Linux (`dd`)

Decompress and write in one pass:

```sh
gzip -dc ados-luckfox-pico-zero-vX.Y.Z.img.gz \
  | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync
sync
```

Replace `/dev/sdX` with the device node of your microSD (use `lsblk`
on Linux or `diskutil list` on macOS to identify it). On macOS the node
is `/dev/rdiskN` for raw access; use `diskutil unmountDisk` first.

### Windows (balenaEtcher)

1. Open balenaEtcher.
2. Select the `.img.gz` directly (Etcher decompresses on the fly).
3. Select the microSD as the target.
4. Click Flash.

Etcher verifies the write automatically. Eject when prompted.

## First boot

1. Insert the microSD into the Luckfox Pico Zero.
2. Connect the USB-UART cable. Open a serial terminal at `115200 8N1`.
3. Apply power. Boot completes in 30 to 60 seconds depending on SD
   speed.
4. Watch the UART for a banner like:

```
==== ADOS PAIR CODE: AB23X4 ====
```

If the board has an OLED or LCD wired to the standard SPI/I2C pins
declared in the board profile, the same code appears on the display.

The code is regenerated on every fresh boot until the device pairs.
Once paired, the code is consumed and the device starts emitting
heartbeats to the cloud relay.

### No UART cable? Use the Wi-Fi AP fallback

If the device boots and does not associate to a Wi-Fi network within
30 seconds, the agent stands up a temporary soft-AP on `wlan0` so an
operator can complete the setup wizard from a phone or laptop.

1. Wait ~45 seconds after applying power.
2. Scan for Wi-Fi networks. A new SSID `ados-XXXX` appears (the four
   characters are the last four hex digits of the device's MAC, in
   lowercase).
3. Join the SSID. The WPA2 passphrase is the **device's pair code**
   (six uppercase letters and digits — the same string that would have
   appeared on UART).
4. Open `http://192.168.4.1:8080` in a browser. The setup webapp
   displays the pair code at the top of the page and walks through
   network configuration.
5. Once the operator picks a real Wi-Fi network in the wizard and
   submits, the agent tears down the soft-AP and reconnects via
   `wpa_supplicant`. The device is then reachable from Mission Control
   on its new IP.

The pair-code-as-passphrase pattern means an attacker would need
physical proximity AND the device's pair code (which never leaves the
operator's hands except via UART or the soft-AP itself) to join the
fallback AP. This is intentional: the AP is meant to bootstrap a
single legitimate operator, not to provide an open hotspot.

## Pair via Mission Control

1. Open Mission Control.
2. Click `Add drone`.
3. Enter the pair code shown on UART or the display.
4. Wait up to 30 seconds for the fleet card to light up.

The drone appears on the fleet view with `runtimeMode: lite` and the
board metadata (`boardName`, `soc`, `ramMb`) from the heartbeat.

## Troubleshooting

### No pair code on UART

- Confirm UART is at 115200 8N1, not 9600.
- Confirm the cable is wired correctly: TX on the board to RX on the
  cable. Crossed wires give silence in both directions.
- Wait the full 60 seconds — first boot regenerates `/etc/machine-id`
  and writes it before the agent starts.
- If still no banner, log in via SSH (the image enables Dropbear by
  default, accept-key at first connect) and check
  `/var/log/ados-agent-lite.log` plus `dmesg | tail -100`.

### AIC8800DC Wi-Fi does not associate

The image bundles the AIC8800DC kernel module. If `iw dev` shows no
interface:

```sh
dmesg | grep -i aic
lsmod | grep -i aic
```

If the module is loaded but no interface comes up, the chip likely
needs the firmware blob in `/lib/firmware/aic8800/`. Confirm that
directory is populated; re-flash if it's empty.

### FC serial not detected

The default config maps the FC to `/dev/ttyS0` at 115200. Verify the
device exists:

```sh
ls -l /dev/ttyS*
ados-agent-lite status
```

If the FC is on a different UART, edit `/etc/ados/agent.yaml` and
restart the service:

```sh
/etc/init.d/S99ados-agent-lite restart
```

### Cloud relay not reachable

The agent retries the cloud relay every 30 seconds while unpaired and
every 5 seconds once paired. If the heartbeat never lands:

```sh
ados-agent-lite status --json
```

Check the `cloud` block for the last error. Typical causes: no DNS
resolution (Wi-Fi associated but no IP yet — wait for DHCP), TLS clock
skew (chrony hasn't synced; wait two minutes after boot), or firewall
blocking port 8883 outbound.

## Recovery

### Re-flash

If the image is corrupt or pairing state is wedged, re-flash from the
same `.img.gz`. The microSD's bootloader, kernel, rootfs, and config
are all replaced in one pass. Pair code is regenerated on the next
boot; previous pairing is invalidated.

### In-place update

For minor agent updates without re-flashing:

```sh
sudo ados-agent-lite update
```

This pulls the latest signed binary, verifies the minisign signature,
and replaces `/usr/local/bin/ados-agent-lite` in place. Configuration
and pairing state are preserved.

### Factory reset (preserve image, reset state)

To force a fresh first-boot surface without re-flashing:

```sh
sudo rm /etc/ados/.first-boot-done /etc/ados/pairing.json
sudo /etc/init.d/S99ados-agent-lite restart
sudo /etc/init.d/S98ados-first-boot start
```

The next reboot prints a fresh pair code on the UART.

## Image contents

For reference, the image bakes in:

- ADOS lite agent binary (Rust, musl-static armv7) at
  `/usr/local/bin/ados-agent-lite`
- busybox sysv-rc init script at `/etc/init.d/S99ados-agent-lite`
- First-boot pairing-code surface at `/etc/init.d/S98ados-first-boot`
- Default agent config at `/etc/ados/agent.yaml`
- minisign for OTA verification
- Dropbear for SSH access
- chrony for NTP sync
- wpa_supplicant + udhcpc for Wi-Fi
- AIC8800DC kernel module + firmware blob

Image size is around 60 MB compressed, 200 MB uncompressed.

## Build your own

OEMs who want to bake custom defaults (Wi-Fi credentials, white-label
branding, pre-configured cloud relay URL) can build a derivative image
locally. The same recipe runs in
`.github/workflows/luckfox-image-build.yml`, so a local build produces
an artifact byte-equivalent to a stamped CI release modulo overlay and
signing key.

### Prerequisites

- Linux build host (Ubuntu 22.04 or newer recommended; macOS works
  via Docker but is slower). Buildroot expects a GNU/Linux toolchain.
- 8 GB free disk for the SDK clone, 16 GB for a full build tree.
- Buildroot host packages:
  ```sh
  sudo apt-get install -y build-essential libncurses5-dev rsync bc \
    cpio python3 python3-pip unzip gzip xz-utils file wget git bison \
    flex minisign
  ```
- A clone of the upstream Luckfox SDK (the BSP, kernel, and base
  Buildroot tree live there; this repo's
  `agents/lite-rs/buildroot/` is the BR2_EXTERNAL overlay only):
  ```sh
  git clone https://github.com/LuckfoxTECH/luckfox-pico ~/luckfox-sdk
  ```
- A minisign keypair if you want signed artifacts (optional; the
  install path warns and skips verify when no signature is present).

### Build steps

1. Wire this repo's `agents/lite-rs/buildroot/` in as the
   `BR2_EXTERNAL` tree:
   ```sh
   export BR2_EXTERNAL=/path/to/ADOSDroneAgent/agents/lite-rs/buildroot
   ```
2. Pick the closest base defconfig from the Luckfox SDK
   (`luckfox_pico_zero_buildroot_defconfig` is the typical match) and
   layer the ADOS additions on top:
   ```sh
   cd ~/luckfox-sdk
   make luckfox_pico_zero_buildroot_defconfig
   echo 'BR2_PACKAGE_ADOS_AGENT_LITE=y' >> .config
   yes "" | make olddefconfig
   ```
3. Drop your overlay (white-label branding, custom TLS roots,
   pre-shared Wi-Fi credentials) into a directory and point
   `BR2_ROOTFS_OVERLAY` at it. The overlay is the right place for any
   per-OEM defaults; the agent code itself does not need a fork.
4. Run the full build:
   ```sh
   make BR2_JLEVEL="$(nproc)"
   ```
   First build is 30 to 60 minutes depending on host CPU and network
   throughput for the upstream package fetch. Subsequent builds with
   `ccache` warm hit in under 10 minutes.
5. Compress and (optionally) sign the resulting image:
   ```sh
   "${BR2_EXTERNAL}/post-image.sh" output/images/sdcard.img
   minisign -S -s ~/.minisign/your.key \
     -m output/images/sdcard.img.gz
   ```

If you sign with your own keypair, replace the vendored public key in
`scripts/install-lite.sh` with the one matching your private key, or
disable verification by setting `ADOS_SKIP_MINISIGN=1` in the install
environment (not recommended for production).
