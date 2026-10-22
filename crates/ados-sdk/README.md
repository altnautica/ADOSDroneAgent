# ados-sdk

[![crates.io](https://img.shields.io/crates/v/ados-sdk.svg)](https://crates.io/crates/ados-sdk)
[![docs.rs](https://img.shields.io/docsrs/ados-sdk)](https://docs.rs/ados-sdk)
[![license](https://img.shields.io/crates/l/ados-sdk.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Build Rust plugins for the ADOS Drone Agent: async IPC client, plugin context, hardware driver traits and lifecycle runner.**

A plugin is a separate process the ADOS Drone Agent starts, sandboxes and talks to over a private Unix socket. This crate is everything a Rust plugin needs on its side of that socket: implement the [`Plugin`](https://docs.rs/ados-sdk/latest/ados_sdk/lifecycle/trait.Plugin.html) hooks you care about, hand the type to [`run_plugin`](https://docs.rs/ados-sdk/latest/ados_sdk/lifecycle/fn.run_plugin.html), and use the [`PluginContext`](https://docs.rs/ados-sdk/latest/ados_sdk/context/struct.PluginContext.html) to publish events, read telemetry, send MAVLink or drive hardware. Every call is checked by the host against the capabilities the plugin's manifest was granted.

```rust,no_run
use std::collections::BTreeMap;

use ados_sdk::{run_plugin, ClientError, Plugin, PluginContext};
use async_trait::async_trait;
use rmpv::Value;

struct Hello;

#[async_trait]
impl Plugin for Hello {
    fn new() -> Self {
        Hello
    }

    async fn on_start(&mut self, ctx: &PluginContext) -> Result<(), ClientError> {
        ctx.events.publish("hello.started", Value::from("hi")).await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    // The host passes the socket path and token on argv and in the environment.
    if let Err(e) = run_plugin::<Hello, _>(env!("CARGO_PKG_VERSION"), BTreeMap::new(), shutdown).await {
        eprintln!("plugin stopped: {e}");
        std::process::exit(1);
    }
}
```

The example needs `async-trait`, `rmpv` and `tokio` (with the `macros`, `rt-multi-thread` and `signal` features) next to `ados-sdk` in your `Cargo.toml`.

## Highlights

- Speaks the same wire as the agent's Python plugin SDK, using the shared [`ados-protocol`](https://crates.io/crates/ados-protocol) codec, so Rust and Python plugins are interchangeable from the host's point of view.
- Every lifecycle hook has a default no-op: a plugin overrides only what it needs.
- Capability tokens rotated by the host are adopted transparently, so a rotation never costs the plugin a failed call.
- Hardware driver traits for cameras, gimbals, lidars, GPS receivers, ESCs and payload actuators.
- A [`testing`](https://docs.rs/ados-sdk/latest/ados_sdk/testing/) module for exercising a plugin without a running agent.

## How it works

- [`PluginIpcClient`](https://docs.rs/ados-sdk/latest/ados_sdk/client/struct.PluginIpcClient.html): the async client. It connects to the plugin socket, runs the `hello` handshake, sends requests keyed by a request id, runs the reader loop and dispatches event and MAVLink pushes to topic-matched callbacks.
- [`PluginContext`](https://docs.rs/ados-sdk/latest/ados_sdk/context/struct.PluginContext.html): the plugin-facing facade, with sub-clients for events, MAVLink, MSP, telemetry, peripherals, camera, config, process, cloud, vision and lifecycle.
- [`drivers`](https://docs.rs/ados-sdk/latest/ados_sdk/drivers/): the hardware driver traits (`CameraDriver`, `GimbalDriver`, `LidarDriver`, `GpsDriver`, `EscDriver`, `PayloadActuatorDriver`) and their candidate, capability and sample types.
- [`Plugin`](https://docs.rs/ados-sdk/latest/ados_sdk/lifecycle/trait.Plugin.html) and [`run_plugin`](https://docs.rs/ados-sdk/latest/ados_sdk/lifecycle/fn.run_plugin.html): the lifecycle trait and the runner. The runner reads `--socket`, `--token` and `--agent-id` from argv or the environment, connects, then calls `on_install`, `on_enable`, `on_configure` and `on_start`, waits for shutdown, and finishes with `on_stop` and `on_disable`. If the host session drops, the runner returns an error so the process exits and the host restarts it with a fresh session.
- [`http`](https://docs.rs/ados-sdk/latest/ados_sdk/http/): for plugins that serve their own HTTP pages, the host-provided socket path and a listener bind helper.

The wire is length-prefixed msgpack envelopes over a per-plugin Unix domain socket, each carrying an HMAC-signed capability token. The agent's capability catalog is re-exported from `ados-protocol` as [`capabilities`](https://docs.rs/ados-sdk/latest/ados_sdk/capabilities/).

## Versioning

This crate is versioned in lockstep with the ADOS Drone Agent: version X.Y.Z speaks the wire of agent X.Y.Z. Depend on the version that matches the agent your plugin targets.

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.
[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

### Other ADOS crates

<!-- ados-family:crates -->
**Platform (versioned with the ADOS Drone Agent)**

| Crate | What it does | Docs |
|---|---|---|
| [`ados-protocol`](https://crates.io/crates/ados-protocol) | Wire contracts of the ADOS Drone Agent: IPC framing, plugin RPC envelope, capability tokens and the vision frame bus. | [docs.rs](https://docs.rs/ados-protocol) |
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
