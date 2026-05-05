# Lite agent deployment guide (OEM)

The lite agent is a single static Rust binary that runs the ADOS Drone
Agent control plane (MAVLink router + cloud relay client + universal
setup wizard) on low-RAM SBCs. It versions independently of the Python
full agent and ships from the same install URL the rest of the project
uses.

This guide covers operator-facing deployment of the lite agent on the
two reference boards.

## Reference boards

| Board | RAM | Arch | Libc | Init | Wi-Fi | Encoder |
|---|---|---|---|---|---|---|
| Luckfox Pico Zero (RV1106G3) | 256 MB | armv7 | uclibc | busybox sysv-rc | AIC8800DC (out-of-tree) | RKMPI hardware H.264/H.265 |
| Raspberry Pi Zero 2 W (BCM2710A1) | 512 MB | aarch64 | glibc | systemd | Cypress CYW43436 | libcamera + V4L2 H.264 |

Other lite-eligible SBCs (≤512 MB RAM) are accepted via the auto-detect
path. Override with `--profile lite` if the board's fingerprint isn't in
the manifest.

## One-line install

The same canonical install URL is used for every supported board. The
script auto-detects the SBC and dispatches to the lite path when
appropriate.

```sh
curl -sSL https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install.sh \
  | sudo bash
```

Optional flags:

- `-s -- --pair PAIRCODE` — install paired against the cloud relay.
  Without this flag the agent boots unpaired and emits a beacon every
  30 seconds with a fresh pair code; the operator pairs through Mission
  Control's "Add drone" dialog.
- `-s -- --profile lite` — force the lite path (useful for low-RAM
  boards not yet in the auto-detect manifest).
- `-s -- --dry-run` — print the detected profile and the install body
  that would run, without making changes.

The script is idempotent. Re-running upgrades the binary in place;
`/etc/ados/agent.yaml`, `/etc/ados/pairing.json`, and any custom
configuration are preserved.

## Prerequisites

The lite installer assumes a working Linux shell on the SBC plus
network reachability to `github.com`. Things the installer does NOT
cover:

- Flashing the SBC image (operator uses `dd` / Etcher / Raspberry Pi
  Imager / `rkdeveloptool` / `upgrade_tool` per their board's docs).
- Wi-Fi credentials, Ethernet config, USB tethering. The board must
  already be on a network the operator can reach.
- SSH or console access setup. The operator uses whatever shell access
  their board supports.
- System updates. The kernel and base packages must be at a recent
  enough version (Linux ≥ 5.4, busybox or coreutils, `tar`, `gzip`,
  `sha256sum` or `shasum`).
- Hardware enablement (UART pin muxing for FC serial, CSI camera tree
  overlays, USB role switch on Luckfox, DKMS for AIC8800DC + 88XXau
  Wi-Fi drivers). These are image-build-time concerns or BSP package
  installs, not install-time concerns.

## Buildroot-specific notes (Luckfox Pico Zero)

Buildroot images ship with `wget` but not `curl`. The lite installer
detects this and falls back automatically. If the system also lacks
both, install one before running the install command.

`/usr/local/bin` may not exist on a minimal Buildroot rootfs. The
installer creates it idempotently.

`tar` on Buildroot is the busybox variant which does not understand
`-z`. The installer extracts via `gzip -dc | tar -x -f -` instead.

`minisign` is not on a stock Buildroot rootfs. The installer logs a
notice and skips signature verification when running with the
placeholder public key (CI release pipeline replaces the placeholder
with the real key on tag releases). To enforce signature verification,
either install minisign onto the rootfs at build time or download a
static minisign binary into `/usr/local/bin/` before re-running the
installer.

## Init system handling

The installer auto-detects the init system and writes the appropriate
unit:

| Init | Path | Service name |
|---|---|---|
| systemd | `/etc/systemd/system/ados-agent-lite.service` | `ados-agent-lite` |
| busybox sysv-rc | `/etc/init.d/S99ados-agent-lite` | `S99ados-agent-lite` |
| OpenRC | `/etc/init.d/ados-agent-lite` | `ados-agent-lite` |
| runit / s6 | board-specific service supervisor directory | per BSP convention |

The agent runs as `root` so it can open the FC serial device and bind
to port 8080 without capabilities setup.

## Pairing

After install, browse to `http://<board-ip>:8080/setup` (mDNS hostname
also works: `http://ados-<device_id>.local:8080/setup` on networks with
mDNS enabled). The wizard walks through profile selection, hardware
check, cloud-choice, and pairing. Alternately, run the CLI:

```sh
sudo ados-agent-lite pair AB23X4
```

The pair code (six characters, ambiguous-glyph-stripped charset) is
shown in the wizard's pairing step or read out of the `pairing_code`
field in the cloud beacon by Mission Control. Codes rotate every 15
minutes; the existing code is preserved within the TTL window.

## Heartbeat and observability

Once paired, the agent posts a heartbeat to the cloud relay every 5
seconds with:

- `runtimeMode: "lite"` — distinguishes the lite agent from the Python
  full agent in the GCS fleet card.
- Static board metadata (`boardName`, `soc`, `arch`, `ramMb`,
  `hostname`, `lastIp`, `mdnsHost`) — populated once at startup.
- Live system metrics (`cpuPct`, `memUsedMb`, `memTotalMb`,
  `socTempC`) — refreshed each tick.

Live agent logs:

| Init | Tail logs |
|---|---|
| systemd | `journalctl -u ados-agent-lite -f` |
| busybox sysv-rc | tail the agent's stdout (depends on how the init script redirects; typically `/var/log/ados-agent-lite.log` if redirected, otherwise visible at the console) |
| OpenRC | `rc-service ados-agent-lite status` and the configured log target |

## Upgrade

In-place upgrade, preserving config + pairing state:

```sh
sudo ados-agent-lite update
```

This re-runs the install script in upgrade mode under the hood. The
agent restarts and picks up the new binary.

## Uninstall

```sh
sudo ados-agent-lite uninstall
```

Stops the service, removes the binary and init unit, and preserves
`/etc/ados/agent.yaml` + `/etc/ados/pairing.json` for a possible
re-install. To wipe state too, also `rm -rf /etc/ados/`.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `curl: command not found` during install | Buildroot image | The installer falls back to `wget` automatically; if neither is present, install one first |
| `tar: invalid option -z` | busybox-tar | Re-run installer; the gzip-pipe path activates automatically when busybox-tar is detected |
| Webapp renders blank `<div id="app">` | Old binary with absolute-path mount bug | Re-run install (the root-mount fallback is in 0.1.0+) |
| `journalctl` not present on busybox | No systemd journal | Tail the configured log file or stdout redirect |
| AIC8800DC Wi-Fi disassociates randomly | Known driver / rfkill interaction on Luckfox | Check `dmesg`; toggle `rfkill unblock all`; consider patching the AIC8800DC DKMS to the latest community fork |
| `ados-agent-lite: command not found` after install | `/usr/local/bin` not on PATH | `export PATH=$PATH:/usr/local/bin` or reload the shell |

## Hardened operations

Defense-in-depth knobs the lite agent exposes for operators who run it
on shared networks or who want strict supply-chain verification on
every upgrade.

### Pinned upgrade script hash

`ados-agent-lite update` fetches the install script from
`raw.githubusercontent.com`, logs its SHA256 at INFO level, and
proceeds. Operators who want strict verification can require an exact
hash and fail the upgrade on mismatch:

```sh
sudo ados-agent-lite update --require-script-sha256 \
  e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
```

The hash is the SHA256 of the install script bytes the agent fetches.
Compute it out of band (`curl -sSL <url> | sha256sum`) when the
operator wires the upgrade into orchestration. When the flag is
omitted the agent falls back to the unverified-but-logged behavior so
emergency in-field upgrades over a flaky link don't fail closed.

The cloudflared binary has its own SHA256 pin baked into the agent
build; cloudflared releases that don't match the expected hash are
rejected at install time.

### Setup surface origin gate

When `api.bind` is set to `0.0.0.0` (the common LAN-wizard path), the
universal setup REST surface enforces a same-origin policy on
mutating methods (POST / PUT / PATCH / DELETE). A POST whose `Origin`
header is foreign to the agent's host is rejected with HTTP 403.

The allowlist is built once at agent startup from the configured
bind address + port and the device_id, and includes:

- `http://<bind_host>:<port>` and the default-port form
- `http://localhost:<port>` and `http://127.0.0.1:<port>`
- `http://ados-<device_id>.local:<port>` (the mDNS hostname)
- `https://` variants of all of the above for reverse-proxy operators

GET / HEAD / OPTIONS requests pass through unchanged. Requests
without an `Origin` header (curl, native HTTP clients, the wizard
webapp's own no-CORS fetches) also pass through. The gate exists to
stop a browser on the same LAN from being weaponized into
reconfiguring the agent via a malicious page.

The allowlist is logged at startup:

```
INFO setup origin allowlist configured bind_host=0.0.0.0 bind_port=8080 device_id=...
```

A change to `api.bind` requires an agent restart for the allowlist to
refresh, matching how every other bind-derived value is handled.

### Graceful shutdown

The agent installs SIGTERM and SIGINT handlers. `systemctl stop
ados-agent-lite` (or the equivalent busybox sysv-rc / OpenRC
operation) drains the active tasks, flushes pending writes, and
exits. The MQTT client publishes a final unpaired-or-paired status
before disconnecting so Mission Control reflects the offline state
without waiting for the heartbeat timeout. On busybox systems where
the init script sends SIGTERM the same flow runs; SIGKILL after the
configured grace period is the fallback.

### Heartbeat fields

The cloud heartbeat now carries:

- `services` — array of `{ name, state }` for the in-process tasks
  (mavlink-router / cloud-client / http-api). Today the array is
  three rows reflecting the lite agent's process surface; future
  phases will add `wfb-tx`, `video-encoder`, etc.
- `fcConnected` — boolean reflecting live MAVLink heartbeat presence
  on the FC serial port. Mission Control reads this for the fleet
  card's link indicator without waiting for a fresh telemetry
  snapshot.

These fields are additive; older Mission Control builds that don't
read them keep working.

## Public-repo discipline

This document ships in the public OSS repo. No partner names, no
upstream-codebase attribution beyond protocol references, no internal
phase tags, no business / pricing / competitor language, no India-only
framing. See operating rules 29 / 30 / 31 / 32 / 33 in the project's
internal documentation for the full discipline.
