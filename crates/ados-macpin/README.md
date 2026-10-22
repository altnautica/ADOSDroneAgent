# ados-macpin

[![crates.io](https://img.shields.io/crates/v/ados-macpin.svg)](https://crates.io/crates/ados-macpin)
[![docs.rs](https://img.shields.io/docsrs/ados-macpin)](https://docs.rs/ados-macpin)
[![license](https://img.shields.io/crates/l/ados-macpin.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Stable MAC addresses for Linux network adapters that randomize theirs every boot: chipset quirk table, deterministic per-machine derivation and systemd-networkd .link files.**

Some USB WiFi chipsets have no MAC burned into efuse, so their driver invents a new random address each time it loads. On a DHCP network that means a new lease and a new IP every boot, which makes the machine hard to find. This crate pins a stable, locally administered MAC derived from `/etc/machine-id`, written as a `systemd-networkd` `.link` drop-in that takes effect on the next boot without touching the live interface. Reach for it on any embedded Linux box with a randomizing adapter.

```rust
use std::path::PathBuf;

use ados_macpin::engine::{adapter_key, link_file_name, render_link_file, PinLayout, NETWORKD_DIR};
use ados_macpin::{derive_pinned_mac, is_quirk_randomizer, UsbId};

// An AIC8800-family USB WiFi adapter: a known randomizer.
let id = UsbId { vid: 0xa69c, pid: 0x8d81 };
assert!(is_quirk_randomizer(id).is_some());

// Same machine-id and USB port in, same MAC out, on every boot.
let mac = derive_pinned_mac("03851cd61fc642d781d3f93a00e624cd", "5-1.3").unwrap();
assert!(mac.is_locally_administered() && mac.is_unicast());

let layout = PinLayout {
    state_path: PathBuf::from("/var/lib/myapp/mac-pins.state"),
    networkd_dir: PathBuf::from(NETWORKD_DIR),
    link_prefix: "10-myapp-mac-".into(),
    owner_label: "myapp service".into(),
};
let key = adapter_key("a69c:8d81", "5-1.3");
let name = link_file_name(&layout, &key);
let body = render_link_file(&layout, "Path=platform-xhci-hcd.0-usb-0:1.3:1.0", &mac);

assert_eq!(name, format!("10-myapp-mac-{}.link", "a69c-8d81@5-1.3"));
assert!(body.contains(&format!("MACAddress={mac}")));
assert!(body.contains("NamePolicy=kernel"));
```

## Highlights

- Deterministic: the MAC is `02:` plus five bytes of `sha256(machine_id:salt)`, so it is stable per machine and distinct across machines and USB ports.
- Refuses unsafe `.link` files: a `[Match]` block that does not name exactly one adapter (a glob, a negation, a bare `Driver=`) is rejected before anything is written.
- Next-boot only: provisioning writes a file and reloads the udev rules database; it never runs `udevadm trigger` on the `net` subsystem and never changes a live address unless you call `apply_live`.
- Every path is yours: the state file, drop-in directory, file prefix and header label come from a `PinLayout`.
- A cross-boot learner flags unknown adapters whose MAC keeps changing, for an operator to confirm.
- Async entry points (`apply_live`, `remove_pin_link`) run `ip` and `udevadm` bounded under `tokio::process` with `kill_on_drop`.

## How it works

Detection does not trust two signals that look authoritative but lie in practice: the kernel's `addr_assign_type` can read `0` (permanent) while the adapter randomizes, and the random MAC can carry a real-looking vendor OUI with the locally administered bit clear. So randomization is identified by chipset VID:PID ([`MAC_QUIRKS`](https://docs.rs/ados-macpin/latest/ados_macpin/constant.MAC_QUIRKS.html)) or by observing the MAC change across boots for a stable device identity. Chipsets in [`KNOWN_STABLE_EFUSE`](https://docs.rs/ados-macpin/latest/ados_macpin/constant.KNOWN_STABLE_EFUSE.html) are never pinned.

An adapter is identified by its model plus the USB port it sits in ([`adapter_key`](https://docs.rs/ados-macpin/latest/ados_macpin/engine/fn.adapter_key.html), `vvvv:pppp@<usb_path>`), which survives kernel renames and enumeration-order changes. [`classify_adapter`](https://docs.rs/ados-macpin/latest/ados_macpin/engine/fn.classify_adapter.html) is the pure decision: an operator override wins, then the stable-efuse allowlist, then the quirk table (pin when enabled, a systemd-udev link mechanism exists and a machine-id is available), then the learner (a candidate after [`LEARN_THRESHOLD`](https://docs.rs/ados-macpin/latest/ados_macpin/engine/constant.LEARN_THRESHOLD.html) MAC changes).

The `.link` file matches on the adapter's udev `ID_PATH` (`Path=`), sets `NamePolicy=kernel` so the interface keeps its name, and sets an explicit `MACAddress=`. That key is unconditional, which is why [`match_block_is_specific`](https://docs.rs/ados-macpin/latest/ados_macpin/engine/fn.match_block_is_specific.html) guards every write: a wildcard match would hand one address to every interface on the machine.

On Linux, [`reconcile`](https://docs.rs/ados-macpin/latest/ados_macpin/engine/fn.reconcile.html) runs the whole pass: enumerate `/sys/class/net`, fold the learner, classify, write or sweep `.link` files and persist a [`MacPinsState`](https://docs.rs/ados-macpin/latest/ados_macpin/struct.MacPinsState.html) JSON document atomically. On other platforms the sysfs functions are stubs, so the pure logic builds and tests anywhere. The `engine` module also has helpers that project the state file to camelCase JSON for a status API.

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.
[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

### Other ADOS crates

<!-- ados-family:crates -->
**Platform (versioned with the ADOS Drone Agent)**

| Crate | What it does | Docs |
|---|---|---|
| [`ados-protocol`](https://crates.io/crates/ados-protocol) | Wire contracts of the ADOS Drone Agent: IPC framing, plugin RPC envelope, capability tokens and the vision frame bus. | [docs.rs](https://docs.rs/ados-protocol) |
| [`ados-sdk`](https://crates.io/crates/ados-sdk) | Write Rust plugins for the ADOS Drone Agent: IPC client, plugin context, driver traits and lifecycle runner. | [docs.rs](https://docs.rs/ados-sdk) |
| [`ados-vision`](https://crates.io/crates/ados-vision) | Single-object visual tracker: Kalman motion model, appearance re-identification and YOLOv8 head decoding. | [docs.rs](https://docs.rs/ados-vision) |
| [`ados-gpio`](https://crates.io/crates/ados-gpio) | Drive status buzzers and LEDs on Linux GPIO lines, with bounded software-PWM beep patterns. | [docs.rs](https://docs.rs/ados-gpio) |
| [`ados-hal-probe`](https://crates.io/crates/ados-hal-probe) | Find out what a board's hardware can really do by probing it, plus bundled board profiles. | [docs.rs](https://docs.rs/ados-hal-probe) |

**Standalone**

| Crate | What it does | Docs |
|---|---|---|
| [`ados-sensorhub`](https://crates.io/crates/ados-sensorhub) | Sensor hub wire protocol: COBS framing, CRC-16 and fixed-layout IMU, baro, mag and GNSS packets. no_std. | [docs.rs](https://docs.rs/ados-sensorhub) |
| [`ados-offload`](https://crates.io/crates/ados-offload) | Choose where perception runs (onboard, offloaded or hybrid) and hold the vehicle when results go stale. | [docs.rs](https://docs.rs/ados-offload) |
| [`ados-lora`](https://crates.io/crates/ados-lora) | LoRa transmit governor with cited regional band plans, duty cycle, dwell time and airtime accounting. | [docs.rs](https://docs.rs/ados-lora) |
| [`ados-crsf-codec`](https://crates.io/crates/ados-crsf-codec) | no_std CRSF / ExpressLRS codec for RC channels, telemetry and parameter frames. | [docs.rs](https://docs.rs/ados-crsf-codec) |
| [`ados-positioning`](https://crates.io/crates/ados-positioning) | Split mixed UBX, NMEA and RTCM3 GNSS streams, configure u-blox receivers and run an RTK base survey-in. | [docs.rs](https://docs.rs/ados-positioning) |
| [`ados-rtcm`](https://crates.io/crates/ados-rtcm) | RTCM3 framing, CRC-24Q and MAVLink GPS_RTCM_DATA fragmentation for RTK corrections. | [docs.rs](https://docs.rs/ados-rtcm) |
| [`ados-clock`](https://crates.io/crates/ados-clock) | Map remote clocks onto the host clock: least-squares and envelope fits, TIMESYNC and a chrony sink. | [docs.rs](https://docs.rs/ados-clock) |
| [`ados-swarm`](https://crates.io/crates/ados-swarm) | Swarm control laws: separation, flocking, formations, CBBA task allocation and a point-mass simulator. | [docs.rs](https://docs.rs/ados-swarm) |
| [`ados-mavlink-tunnel`](https://crates.io/crates/ados-mavlink-tunnel) | Send large messages over MAVLink TUNNEL frames with chunking and bounded reassembly. | [docs.rs](https://docs.rs/ados-mavlink-tunnel) |
| [`ados-display-driver`](https://crates.io/crates/ados-display-driver) | Small-display output on Linux: SSD1306/SH1106 OLED over I2C, pixel packers and a framebuffer writer. | [docs.rs](https://docs.rs/ados-display-driver) |
| [`ados-hmi`](https://crates.io/crates/ados-hmi) | Front-panel input: touchscreen calibration and gestures, button presses and rotary encoders. | [docs.rs](https://docs.rs/ados-hmi) |
<!-- /ados-family:crates -->

### ADOS repositories

<!-- ados-family:repos -->
| Repo | Runs on | What it does | Link |
|---|---|---|---|
| ADOS Mission Control | Browser, desktop (Electron) | Ground control station: FC setup, mission planning, flight, 3D simulation, fleet | [ADOSMissionControl](https://github.com/altnautica/ADOSMissionControl) |
| ADOS Drone Agent | Companion computer on the drone, or a ground node | Onboard agent: MAVLink routing, HD video, long-range radio link, plugins, remote control | [ADOSDroneAgent](https://github.com/altnautica/ADOSDroneAgent) |
| ADOS Android GCS | Android phones and tablets | Native ground control app for agents on the LAN, direct flight controllers and the cloud | [ADOSAndroidGCS](https://github.com/altnautica/ADOSAndroidGCS) |
| ADOS Extensions | Drone Agent and Mission Control | First-party signed plugins, each with an agent half and a GCS half | [ADOSExtensions](https://github.com/altnautica/ADOSExtensions) |
| ADOS MCP | Your machine | Model Context Protocol server that lets AI clients operate ADOS drones under scopes and a safety gate | [ADOS-MCP](https://github.com/altnautica/ADOS-MCP) |
| Documentation | docs.altnautica.com | Public docs for every ADOS product | [Documentation](https://github.com/altnautica/Documentation) |
<!-- /ados-family:repos -->

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
