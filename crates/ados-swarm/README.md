# ados-swarm

[![crates.io](https://img.shields.io/crates/v/ados-swarm.svg)](https://crates.io/crates/ados-swarm)
[![docs.rs](https://img.shields.io/docsrs/ados-swarm)](https://docs.rs/ados-swarm)
[![license](https://img.shields.io/crates/l/ados-swarm.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Swarm control laws for multirotors: separation, braking-aware barriers, alpha-lattice flocking, formations, CBBA task allocation and a deterministic point-mass simulator.**

Flying several multirotors near each other needs three things at once: never let two aircraft close inside a safety floor, keep the group moving together or holding a shape, and agree on who does which task without a central coordinator. This crate is those control laws as plain Rust, plus a deterministic simulator to test them on a laptop. Reach for it when you are building swarm autonomy on top of any autopilot that accepts position or velocity setpoints.

```rust
use ados_swarm::separation::SEPARATION_HARD_M;
use ados_swarm::sim::{SimDrone, SimWorld};
use ados_swarm::{GeoOrigin, Ned, SwarmControlConfig};

// Three drones in flocking mode, launched 12 m apart at 30 m altitude.
let cfg = SwarmControlConfig {
    enabled: true,
    mode: "flocking".into(),
    ..Default::default()
};
let slots = [1, 2, 3];
let drones = vec![
    SimDrone::new(1, Ned::new(0.0, 0.0, -30.0), &cfg, &slots),
    SimDrone::new(2, Ned::new(12.0, 0.0, -30.0), &cfg, &slots),
    SimDrone::new(3, Ned::new(0.0, 12.0, -30.0), &cfg, &slots),
];
let mut world = SimWorld::new(GeoOrigin::new(47.397, 8.545, 0.0), drones);

// 30 simulated seconds at the 10 Hz control rate.
let report = world.run(300);
println!("closest approach: {:.1} m", report.min_pairwise_m);
assert!(report.min_pairwise_m > SEPARATION_HARD_M);
```

## Highlights

- Every control law is a pure function over a slice of neighbours in a local NED frame: no clock, no socket, no radio.
- One dependency (`serde`, for the configuration structs).
- Deterministic simulator with a first-order velocity plant, 2 Hz beacon quantisation and dead reckoning; four flight scenarios (collision course, 8-drone flocking to a target, wedge station keeping, beacon loss) run as ordinary tests and as `cargo run --example swarm_scenarios`.
- A fixed precedence ladder decides which law flies, and the active level packs into three bits of a one-byte beacon status field.
- Bounded work per tick: nearest-neighbour search is allocation-free and every law weights at most a fixed number of neighbours.

## How it works

### Shape

Every law takes a slice of [`NeighborState`](https://docs.rs/ados-swarm/latest/ados_swarm/neighbor/struct.NeighborState.html) and returns a velocity or position intent. The only stateful piece is [`SwarmController`](https://docs.rs/ados-swarm/latest/ados_swarm/controller/struct.SwarmController.html), which holds the latches a safety layer needs (the hard-override dwell, the staleness deadline, the climb datum) and converts between geodetic coordinates and the local frame at the boundary. Its `tick` consumes this drone's own state plus geodetic neighbour fixes and returns at most one [`Setpoint`](https://docs.rs/ados-swarm/latest/ados_swarm/setpoint/struct.Setpoint.html), the field set of MAVLink `SET_POSITION_TARGET_GLOBAL_INT`. Encoding and sending it is left to the host.

### Modules

| Module | What it holds |
|---|---|
| [`separation`](https://docs.rs/ados-swarm/latest/ados_swarm/separation/index.html) | Repulsive separation and the hard-breach override with a slot-ordered deconfliction climb |
| [`barrier`](https://docs.rs/ados-swarm/latest/ados_swarm/barrier/index.html) | Closing-rate barrier on the braking-adjusted margin, so a vehicle with velocity lag still stops outside the floor |
| [`flocking`](https://docs.rs/ados-swarm/latest/ados_swarm/flocking/index.html) | Alpha-lattice flocking: cohesion, alignment and a saturating virtual leader |
| [`formation`](https://docs.rs/ados-swarm/latest/ados_swarm/formation/index.html) | Five named formations generated from the slot set, anchored on the centroid or a slot |
| [`cbba`](https://docs.rs/ados-swarm/latest/ados_swarm/cbba/index.html) | Consensus-Based Bundle Algorithm: greedy bundle building and the conflict-resolution table |
| [`precedence`](https://docs.rs/ados-swarm/latest/ados_swarm/precedence/index.html) | The mode ladder and its beacon encoding |
| [`status`](https://docs.rs/ados-swarm/latest/ados_swarm/status/index.html) | Beacon status bits: armed, guided, emergency, GPS OK, hero, precedence |
| [`geo`](https://docs.rs/ados-swarm/latest/ados_swarm/geo/index.html) | Flat-earth equirectangular projection around a local origin |
| [`config`](https://docs.rs/ados-swarm/latest/ados_swarm/config/index.html) | Deserializable gains as integer percentages, converted once to sanitised tunings |
| [`sim`](https://docs.rs/ados-swarm/latest/ados_swarm/sim/index.html), [`scenarios`](https://docs.rs/ados-swarm/latest/ados_swarm/scenarios/index.html) | The point-mass world and the four reference scenarios |

### The precedence ladder

Highest first: hard separation, operator directive, formation, flocking, hold. The ACTIVE level, not the commanded one, is what [`SwarmController::precedence`](https://docs.rs/ados-swarm/latest/ados_swarm/controller/struct.SwarmController.html) reports and what [`ModePrecedence::as_status_bits`](https://docs.rs/ados-swarm/latest/ados_swarm/precedence/enum.ModePrecedence.html) packs into status bits 5..7. A drone whose separation layer has taken over reads `hard-separation`, never the mode somebody asked for. `Hold` is encoding zero, so a node with no control layer running reads `hold` truthfully.

### Staleness

[`NEIGHBOR_STALE`](https://docs.rs/ados-swarm/latest/ados_swarm/constant.NEIGHBOR_STALE.html) (3 s) is how long a silent neighbour is kept and how long an empty table is tolerated before the controller stops commanding. [`OWN_STATE_STALE`](https://docs.rs/ados-swarm/latest/ados_swarm/constant.OWN_STATE_STALE.html) (1 s) is tighter, because a frozen own fix stays plausible while every neighbour is measured from it. A transport that prunes its table or gates its own beacon should use the same constants.

### What is deliberately not here

- No leader election. Electing a leader among drones over a lossy broadcast invites a split brain for no benefit; the operator is the single authority.
- No failsafe of its own. The autopilot's own failsafes and geofence are the backstop. When the controller has nothing trustworthy to fly on it emits nothing and lets the flight controller hold.

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
| [`ados-macpin`](https://crates.io/crates/ados-macpin) | Stable MAC addresses for Linux network adapters that randomize theirs every boot. | [docs.rs](https://docs.rs/ados-macpin) |
| [`ados-lora`](https://crates.io/crates/ados-lora) | LoRa transmit governor with cited regional band plans, duty cycle, dwell time and airtime accounting. | [docs.rs](https://docs.rs/ados-lora) |
| [`ados-crsf-codec`](https://crates.io/crates/ados-crsf-codec) | no_std CRSF / ExpressLRS codec for RC channels, telemetry and parameter frames. | [docs.rs](https://docs.rs/ados-crsf-codec) |
| [`ados-positioning`](https://crates.io/crates/ados-positioning) | Split mixed UBX, NMEA and RTCM3 GNSS streams, configure u-blox receivers and run an RTK base survey-in. | [docs.rs](https://docs.rs/ados-positioning) |
| [`ados-rtcm`](https://crates.io/crates/ados-rtcm) | RTCM3 framing, CRC-24Q and MAVLink GPS_RTCM_DATA fragmentation for RTK corrections. | [docs.rs](https://docs.rs/ados-rtcm) |
| [`ados-clock`](https://crates.io/crates/ados-clock) | Map remote clocks onto the host clock: least-squares and envelope fits, TIMESYNC and a chrony sink. | [docs.rs](https://docs.rs/ados-clock) |
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
