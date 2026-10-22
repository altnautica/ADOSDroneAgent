# ados-lora-governor

[![crates.io](https://img.shields.io/crates/v/ados-lora-governor.svg)](https://crates.io/crates/ados-lora-governor)
[![docs.rs](https://img.shields.io/docsrs/ados-lora-governor)](https://docs.rs/ados-lora-governor)
[![license](https://img.shields.io/crates/l/ados-lora-governor.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**LoRa transmit governor with cited regional band plans: time-on-air, duty cycle, dwell time, listen-before-talk, power limits and a persisted airtime ledger.**

Sub-GHz LoRa bands are licence-exempt only while every transmitter keeps to its region's limits on duty cycle, dwell time, power and channel access. This crate decides, frame by frame, whether a transmission is legal right now, on which channel and at what power, and charges the airtime to an append-only ledger that survives crashes and reboots. Reach for it when a device sends LoRa frames and must stay inside the band plan without a human watching.

```rust
use ados_lora_governor::airtime::Modem;
use ados_lora_governor::band::ModuleCaps;
use ados_lora_governor::governor::power::Antenna;
use ados_lora_governor::governor::{max_window_s, Governor, Setup};
use ados_lora_governor::ledger::{Ledger, Now};
use ados_lora_governor::regions::resolve;

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

- [`regions`](https://docs.rs/ados-lora-governor/latest/ados_lora_governor/regions/): the compiled regional band-plan table and region resolution. A row that ships with rules transmits under them; a row whose limits could not all be cited from the regulator text ships with its bands only and is receive-only. Resolution takes an explicit region code first, else a country code mapped to a plan, else none (transmit blocked with `region_unset`, receive still on).
- [`band`](https://docs.rs/ados-lora-governor/latest/ados_lora_governor/band/): what the fitted radio can tune and whether that meets a plan. A plan is supported when at least one tunable channel fits wholly inside a plan sub-band, on the plan's channel grid where it has one, with at least the plan's minimum hop-channel count, and with ambient RSSI available where the plan requires listen-before-talk. Otherwise transmit is blocked with `module_band_mismatch`.
- [`airtime`](https://docs.rs/ados-lora-governor/latest/ados_lora_governor/airtime/): time on air.

  ```text
  T_sym = 2^SF / BW
  T_pre = (N_pre + 4.25) * T_sym
  N_pay = 8 + max(ceil((8 PL - 4 SF + 28 + 16 CRC - 20 IH) / (4 (SF - 2 DE))) * (CR + 4), 0)
  T_oa  = T_pre + N_pay * T_sym
  ```

  Low data-rate optimisation (`DE`) is on when `T_sym >= 16 ms`, which is SF11 and SF12 at 125 kHz.
- [`ledger`](https://docs.rs/ados-lora-governor/latest/ados_lora_governor/ledger/): the persisted airtime ledger. One JSON line per admitted frame, plus a heartbeat line every minute that bounds how much downtime is credited. On load, entries from this boot keep their monotonic time; entries from a previous boot are aged by UTC when both clocks are trusted, else placed at the last trusted heartbeat, else treated as occurring at start. A torn last line is dropped; any other corrupt line moves the file aside and starts every duty rule exhausted for one full window.
- [`governor`](https://docs.rs/ados-lora-governor/latest/ados_lora_governor/governor/): the admit pipeline. [`Governor::admit`](https://docs.rs/ados-lora-governor/latest/ados_lora_governor/governor/struct.Governor.html#method.admit) runs region, module band, `max_power`, `dwell_time` and `duty_cycle` against the plan and the ledger; the first refusal wins. Listen-before-talk needs the radio, so the caller runs it on the granted channel and reports the outcome with `commit` or `refuse`. A commit charges the ledger before the frame is sent.

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.
[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
