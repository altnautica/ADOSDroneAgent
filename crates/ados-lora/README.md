# ados-lora

[![crates.io](https://img.shields.io/crates/v/ados-lora.svg)](https://crates.io/crates/ados-lora)
[![docs.rs](https://img.shields.io/docsrs/ados-lora)](https://docs.rs/ados-lora)
[![license](https://img.shields.io/crates/l/ados-lora.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**LoRa transmit governor with cited regional band plans: time-on-air, duty cycle, dwell time, listen-before-talk, power limits and a persisted airtime ledger.**

Sub-GHz LoRa bands are licence-exempt only while every transmitter keeps to its region's limits on duty cycle, dwell time, power and channel access. This crate decides, frame by frame, whether a transmission is legal right now, on which channel and at what power, and charges the airtime to an append-only ledger that survives crashes and reboots. Reach for it when a device sends LoRa frames and must stay inside the band plan without a human watching.

```rust
use ados_lora::airtime::Modem;
use ados_lora::band::ModuleCaps;
use ados_lora::governor::power::Antenna;
use ados_lora::governor::{max_window_s, Governor, Setup};
use ados_lora::ledger::{Ledger, Now};
use ados_lora::regions::resolve;

// Resolve the EU868 plan from an explicit region code.
let resolved = resolve(Some("EU"), None).expect("EU has a plan");
assert_eq!(resolved.plan.plan_id, "EU868");

let now = Now { mono_ns: 0, utc_ms: 1_760_000_000_000, utc_trusted: false };
let dir = std::env::temp_dir().join(format!("lora-gov-doc-{}", std::process::id()));
std::fs::create_dir_all(&dir).unwrap();
let window = max_window_s(Some(resolved.plan));
let (ledger, _report) = Ledger::open(&dir.join("airtime.jsonl"), "boot-1", now, window).unwrap();

let mut gov = Governor::new(
    Setup {
        resolved: Some(resolved),
        caps: ModuleCaps::any(),
        modem: Modem::SF9_BW125,
        airtime_factor: 1,
        freq_mhz: None,
        erp_mw: None,
        antenna: Antenna { gain_dbi: 2.15, cable_loss_db: 0.0 },
        node_id: 7,
    },
    ledger,
);

// Ask to send 20 bytes on 868.1 MHz (sub-band M: 1 % duty, 25 mW ERP).
let grant = gov.admit(20, Some(868.1), now).expect("admitted");
assert_eq!(grant.sub_band, "868.0-868.6");
gov.commit(&grant, now).unwrap(); // charge the ledger before transmitting
gov.note_sent();
println!("{} MHz at {} dBm, {} us on air", grant.channel_mhz, grant.power_dbm, grant.airtime_us);
# std::fs::remove_dir_all(&dir).ok();
```

## Highlights

- Compiled band-plan table where every enforced number carries the regulator document it comes from.
- Exact LoRa time-on-air from the Semtech SX1261/2 datasheet formula.
- Charge-then-send ledger: each admitted frame is appended and fsync'd before it is sent, so a crash over-counts and never under-counts, and a reboot cannot reset the duty budget.
- Fixed check order with a named refusal reason for each rule (region, module band, power, dwell, duty, listen-before-talk).
- Three runtime dependencies: `serde`, `serde_json`, `tracing`.

## How it works

Modules:

- [`regions`](https://docs.rs/ados-lora/latest/ados_lora/regions/): the compiled regional band-plan table and region resolution. A row that ships with rules transmits under them; a row whose limits could not all be cited from the regulator text ships with its bands only and is receive-only. Resolution takes an explicit region code first, else a country code mapped to a plan, else none (transmit blocked with `region_unset`, receive still on).
- [`band`](https://docs.rs/ados-lora/latest/ados_lora/band/): what the fitted radio can tune and whether that meets a plan. A plan is supported when at least one tunable channel fits wholly inside a plan sub-band, on the plan's channel grid where it has one, with at least the plan's minimum hop-channel count, and with ambient RSSI available where the plan requires listen-before-talk. Otherwise transmit is blocked with `module_band_mismatch`.
- [`airtime`](https://docs.rs/ados-lora/latest/ados_lora/airtime/): time on air.

  ```text
  T_sym = 2^SF / BW
  T_pre = (N_pre + 4.25) * T_sym
  N_pay = 8 + max(ceil((8 PL - 4 SF + 28 + 16 CRC - 20 IH) / (4 (SF - 2 DE))) * (CR + 4), 0)
  T_oa  = T_pre + N_pay * T_sym
  ```

  Low data-rate optimisation (`DE`) is on when `T_sym >= 16 ms`, which is SF11 and SF12 at 125 kHz.
- [`ledger`](https://docs.rs/ados-lora/latest/ados_lora/ledger/): the persisted airtime ledger. One JSON line per admitted frame, plus a heartbeat line every minute that bounds how much downtime is credited. On load, entries from this boot keep their monotonic time; entries from a previous boot are aged by UTC when both clocks are trusted, else placed at the last trusted heartbeat, else treated as occurring at start. A torn last line is dropped; any other corrupt line moves the file aside and starts every duty rule exhausted for one full window.
- [`governor`](https://docs.rs/ados-lora/latest/ados_lora/governor/): the admit pipeline. [`Governor::admit`](https://docs.rs/ados-lora/latest/ados_lora/governor/struct.Governor.html#method.admit) runs region, module band, `max_power`, `dwell_time` and `duty_cycle` against the plan and the ledger; the first refusal wins. Listen-before-talk needs the radio, so the caller runs it on the granted channel and reports the outcome with `commit` or `refuse`. A commit charges the ledger before the frame is sent.

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
