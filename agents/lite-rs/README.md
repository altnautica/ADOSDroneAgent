# Lite Rust Agent

A lightweight Rust implementation of the ADOS Drone Agent for low-RAM SBCs (256–512 MB class). Sits next to the Python full agent at `src/ados/`. Both ship from this repository and speak the same protocol contracts under `proto/`.

## Why a separate implementation

The Python full agent runs ~330 MB resident across multiple supervised services, dominated by per-process CPython interpreter overhead. That cost is invisible on 1 GB+ class boards (Pi 4B, Rock 5C Lite) but does not fit a 256 MB SBC.

The lite agent is a single static Rust binary that handles the control-plane essentials: MAVLink router, cloud relay client (MQTT + HTTPS heartbeat), basic REST API stub, and the four-command CLI surface. It is sized to fit a 256 MB rootfs with comfortable headroom and to share protocol contracts with the Python full agent so cloud relay and ground control station treat the two backends interchangeably.

## Scope at v0.1

In:

- MAVLink router (FC serial → in-process broadcast → cloud forward + LAN listeners)
- Cloud relay client (MQTT-over-TLS for telemetry, HTTPS heartbeat + pairing beacon)
- REST API stub at port 8080 (`/api/v1/setup/status` returns minimal SetupStatus)
- CLI matching the public four-command surface (`ados`, `ados status`, `ados update`, `ados uninstall`)
- HAL board YAML auto-detect via the existing 18+ board profiles at `src/ados/hal/boards/`

Deferred to later versions:

- Video pipeline (libcamera + V4L2 on Pi-class boards; vendor-subprocess for RKMPI on Rockchip)
- WFB-ng air-side orchestration (RTL8812EU dongle hot-plug + `wfb_tx` subprocess)
- Full setup webapp REST handlers (axum implementation of all 12 routes from `proto/setup/`)
- Buildroot package recipe + flashable SBC image

## Layout

```
agents/lite-rs/
├── Cargo.toml                 # workspace root
├── manifest.yaml              # profile metadata for installer + CI
├── README.md                  # this file
├── HAL-EXTENSION.md           # board YAML schema additions
├── crates/
│   ├── ados-agent-lite/       # main binary
│   ├── ados-mavlink/          # MAVLink router crate
│   └── ados-cloud/            # MQTT + HTTPS client crate
└── boards/                    # board-specific init scripts (added per board as needed)
```

## Building

```sh
# Local development on host (x86_64-musl or aarch64-gnu)
cargo build --release

# Cross-compile via the CI-supported targets
cross build --release --target aarch64-unknown-linux-gnu       # Pi Zero 2 W, Pi 4B
cross build --release --target armv7-unknown-linux-musleabihf  # Luckfox Pico Zero, RV1103
cross build --release --target aarch64-unknown-linux-musl      # general aarch64 musl
cross build --release --target x86_64-unknown-linux-musl       # dev container testing
```

Stripped release binaries target 12–15 MB.

## Installation

Operators run the standard installer with `ADOS_PROFILE=lite-rs` to fetch the matching prebuilt binary instead of installing the Python agent:

```sh
ADOS_PROFILE=lite-rs curl -sSL https://github.com/altnautica/ADOSDroneAgent/raw/main/scripts/install.sh | sudo bash -s -- <PAIR_CODE>
```

The installer detects target architecture and libc flavor, downloads the matching signed release tarball from GitHub Releases, verifies the minisign signature plus SHA256 checksum, extracts to `/usr/local/bin/ados-agent-lite`, and installs the appropriate init unit (busybox sysv-rc on Buildroot rootfs, systemd unit on Raspberry Pi OS / Debian).

## Protocol parity

The lite agent speaks the contracts under `proto/` byte-for-byte with the Python full agent. MQTT topics, HTTP heartbeat shape, RTSP push paths, `state.sock` wire format, IPC framing, and CLI output are all stable and shared. The cloud relay and the ground control station treat agents transparently regardless of which backend is running.
