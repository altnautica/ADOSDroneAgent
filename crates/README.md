# ADOS Rust crates

This workspace builds the Rust half of the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent). The libraries listed below are published on crates.io and can be used outside ADOS. The rest of the workspace is the agent's own services, which are not published.

## Published crates

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
| [`ados-macpin`](https://crates.io/crates/ados-macpin) | Stable MAC addresses for Linux network adapters that randomize theirs every boot. | [docs.rs](https://docs.rs/ados-macpin) |
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

Add one with `cargo add <crate>`. Each crate's README is also its docs.rs front page, and its example runs as a doctest.

## Versioning

- **Platform crates** carry the ADOS Drone Agent version. Release X.Y.Z of `ados-sdk` speaks the wire protocol of agent X.Y.Z, and the platform crates depend on each other at that exact version.
- **Standalone crates** have no dependency on another ADOS crate. They follow their own semver, starting at 0.1.0, and are released only when they change.
- A change to any file a published crate packages needs a version bump in the same change. `scripts/check-crates.sh` fails when a crate's packaged files differ from its last published `crates/<name>-v<version>` tag without one.
- Publishing runs from `.github/workflows/crates.yml`: every `name@version` that crates.io does not have yet is published and tagged.

## Workspace-internal crates

These are the agent's services and the glue between them. None of them is published.

| Crate | What it is |
|---|---|
| `ados-supervisor` | Gates, starts and supervises the agent's systemd service units |
| `ados-control` | Native HTTP control API the GCS speaks, on the local socket and the LAN port |
| `ados-mavlink-router` | Owns the flight-controller link, fans out MAVLink and publishes vehicle state |
| `ados-radio` | Long-range radio link manager: adapters, monitor mode, frequency hopping, watchdogs |
| `ados-video` | Video pipeline: encoder, local streaming and radio video, under process-group ownership |
| `ados-vision-engine` | Vision engine service: frame ring, inference backends, plugin socket |
| `ados-plugin-host` | Plugin RPC host: per-plugin sockets, token handshake and the capability gate |
| `ados-cloud` | Opt-in cloud relay: heartbeat, command poll, MAVLink and WebRTC signaling relays |
| `ados-groundlink` | Ground-station data plane: radio receive, video fan-out and the local mesh |
| `ados-net` | Ground-station uplink failover and reachability probing |
| `ados-config` | Agent config loading that reports malformed files, and the node profile |
| `ados-config-tunnel` | Reads and writes the drone's config over the radio through MAVLink TUNNEL |
| `ados-crsf` | CRSF / ExpressLRS RC lane: serial transport, channel transmitter and lane state |
| `ados-hid` | Front-panel buttons, touch and input device services |
| `ados-display` | LCD page UI, status OLED service and the boot-time display probe |
| `ados-swarmbus` | Decentralized swarm state bus: position and velocity beacons every node broadcasts |
| `ados-sensorbus` | The agent's sensor bus: frame codec, clients and capture-contract message types |
| `ados-clock-service` | Host clock service: brokers the sensor bus and fits every time source |
| `ados-sensorhub-service` | Sensor hub service: reads the hub's packet stream over USB or TCP and publishes it |
| `ados-gnss` | GNSS service: receiver sources, RTK base and rover, RTCM out |
| `ados-lora-service` | LoRa beacon and mesh lane service |
| `ados-logd` | Durable local log and telemetry store on SQLite |
| `ados-installer` | Installer that turns a freshly flashed board into a paired, running agent |
| `ados-tui` | Terminal dashboard launched by `ados` |
| `ados-capabilities-codegen` | Generates the capability catalog and contracts for Python, Rust and TypeScript |

## ADOS repositories

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

The published crates are licensed under either of [Apache License, Version 2.0](https://www.apache.org/licenses/LICENSE-2.0) or [MIT license](https://opensource.org/licenses/MIT) at your option, except `ados-sensorhub`, which is MIT only. Each crate directory carries its licence files. The workspace-internal crates and the rest of the ADOS Drone Agent are licensed under GPL-3.0-or-later.
