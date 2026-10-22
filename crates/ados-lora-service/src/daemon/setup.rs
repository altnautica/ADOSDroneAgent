//! Bring-up: config -> region -> radio -> governor.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Value;

use super::lane::{AnyTransport, Lane};
use super::{beacon_interval_s, now_at, utc_trusted};
use crate::config::LoraConfig;
use crate::status::{LaneState, MeshtasticState};
use crate::transport::e22::E22;
use crate::transport::loopback::Loopback;
use crate::transport::meshtastic::Meshtastic;
use crate::transport::{Transport, TransportKind};
use ados_lora::band::{self, ModuleCaps};
use ados_lora::governor::power::{conducted_cap_dbm, pick_step, Antenna};
use ados_lora::governor::{max_window_s, Governor, Setup};
use ados_lora::ledger::Ledger;
use ados_lora::regions::{self, Plan, Resolved};

/// A built lane, ready to run.
pub struct Built {
    pub primary: Lane,
    pub bridge: Option<Lane>,
    pub lane_state: LaneState,
    pub interval_s: u32,
    pub origin: Instant,
    pub origin_mono_ns: i64,
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// `CLOCK_MONOTONIC` now, in ns.
fn monotonic_ns() -> io::Result<i64> {
    let ts =
        nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_err(io::Error::from)?;
    Ok(ts.tv_sec() * 1_000_000_000 + ts.tv_nsec())
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// The band each Meshtastic region preset occupies (firmware
/// `RadioInterface.cpp` region table).
fn meshtastic_band(preset: &str) -> Option<(f64, f64)> {
    Some(match preset {
        "IN" => (865.0, 867.0),
        "EU_868" => (869.4, 869.65),
        "EU_433" => (433.0, 434.0),
        "RU" => (868.7, 869.2),
        "US" => (902.0, 928.0),
        "ANZ" => (915.0, 928.0),
        "ANZ_433" => (433.05, 434.79),
        "BR_902" => (902.0, 907.5),
        "JP" => (920.5, 923.5),
        "KR" => (920.0, 923.0),
        "TW" => (920.0, 925.0),
        "MY_919" => (919.0, 924.0),
        "TH" => (920.0, 925.0),
        _ => return None,
    })
}

/// The channel a Meshtastic node's slot is accounted on: the middle of the
/// overlap of its preset band with the plan sub-band it overlaps most.
pub fn meshtastic_channel(plan: &Plan, preset: &str) -> Option<f64> {
    let (lo, hi) = meshtastic_band(preset)?;
    plan.sub_bands
        .iter()
        .map(|s| (s.lo_mhz.max(lo), s.hi_mhz.min(hi)))
        .filter(|(a, b)| b > a)
        .max_by(|x, y| (x.1 - x.0).total_cmp(&(y.1 - y.0)))
        .map(|(a, b)| (a + b) / 2.0)
}

async fn open_transport(cfg: &LoraConfig, kind: TransportKind) -> io::Result<AnyTransport> {
    Ok(match kind {
        TransportKind::E22 => AnyTransport::E22(Box::new(E22::open(cfg).await?)),
        TransportKind::Meshtastic => {
            AnyTransport::Meshtastic(Box::new(Meshtastic::open(cfg).await?))
        }
        TransportKind::Loopback => AnyTransport::Loopback(Loopback::open(cfg).await?),
    })
}

fn lane(
    cfg: &LoraConfig,
    transport: AnyTransport,
    resolved: Option<Resolved>,
    freq_mhz: Option<f64>,
    ledger_path: &Path,
    now: ados_lora::ledger::Now,
) -> io::Result<Lane> {
    let window = max_window_s(resolved.map(|r| r.plan));
    let (ledger, report) = Ledger::open(ledger_path, &boot_id(), now, window)?;
    if report.reset {
        tracing::warn!(path = %ledger_path.display(), "lora.ledger_reset");
    }
    let setup = Setup {
        resolved,
        caps: transport.caps().clone(),
        modem: transport.modem(),
        airtime_factor: transport.airtime_factor(),
        freq_mhz,
        erp_mw: cfg.erp_mw,
        antenna: Antenna {
            gain_dbi: cfg.antenna_gain_dbi,
            cable_loss_db: cfg.cable_loss_db,
        },
        node_id: cfg.node_id,
    };
    let governor = Governor::new(setup, ledger);
    if let Some(block) = governor.static_block() {
        tracing::warn!(reason = block.reason(), "lora_tx_blocked");
    }
    Ok(Lane {
        transport,
        governor,
        node_id: cfg.node_id,
        rx_frames: 0,
        rx_crc_errors: 0,
        last_heard: None,
    })
}

/// The Meshtastic node's transmit slot under `plan`: the accounted channel and
/// the plan clamp as a conducted step for its `tx_power`. `None` when the node
/// cannot transmit inside the plan (no preset, no overlapping sub-band, a
/// plan it cannot meet, or no power step under the clamp): the node is then
/// configured with transmit off.
pub fn meshtastic_slot(
    plan: &Plan,
    preset: Option<&'static str>,
    caps: &ModuleCaps,
    bw: u32,
    ant: &Antenna,
) -> Option<(&'static str, f64, i8)> {
    let preset = preset?;
    if !band::supported(plan, caps, bw) {
        return None;
    }
    let f = meshtastic_channel(plan, preset)?;
    let rule = plan.sub_band_for(f, bw)?.power?;
    let step = pick_step(&caps.power_steps_dbm, conducted_cap_dbm(&rule, ant, bw), 0)?;
    Some((preset, f, step))
}

/// Configure a Meshtastic node for the resolved plan; returns the accounted
/// channel, or `None` with the node's transmit turned off.
async fn apply_meshtastic(
    m: &mut Meshtastic,
    resolved: Option<Resolved>,
    ant: &Antenna,
) -> io::Result<Option<f64>> {
    let slot = resolved
        .and_then(|r| meshtastic_slot(r.plan, r.plan.meshtastic, m.caps(), m.modem().bw_khz, ant));
    match slot {
        Some((preset, f, step)) => {
            m.apply_plan(Some(preset), step).await?;
            Ok(Some(f))
        }
        None => {
            tracing::warn!("lora_meshtastic_tx_off: the node cannot transmit inside the plan");
            m.apply_plan(None, 0).await?;
            Ok(None)
        }
    }
}

/// Read the config and build the lane; `Ok(None)` when it is not enabled.
pub async fn build(config_path: &Path) -> io::Result<Option<Built>> {
    let cfg: LoraConfig = ados_config::schema::load_slice(config_path, "/lora").map_err(invalid)?;
    if !cfg.enabled {
        return Ok(None);
    }
    let regulatory: Value =
        ados_config::schema::load_slice(config_path, "/network/regulatory").map_err(invalid)?;
    let country = regulatory.get("region").and_then(Value::as_str);
    let resolved = regions::resolve(cfg.region.as_deref(), country);
    let kind = TransportKind::parse(&cfg.transport)
        .ok_or_else(|| invalid(format!("unknown lora.transport {:?}", cfg.transport)))?;
    crate::validate::check_strapped_e22(&cfg.transport, cfg.e22.strapped(), cfg.freq_mhz)
        .map_err(invalid)?;
    let origin = Instant::now();
    let origin_mono_ns = monotonic_ns()?;
    let now = now_at(origin, origin_mono_ns, utc_trusted());
    let ant = Antenna {
        gain_dbi: cfg.antenna_gain_dbi,
        cable_loss_db: cfg.cable_loss_db,
    };
    let preset = resolved.and_then(|r| r.plan.meshtastic);

    let mut transport = open_transport(&cfg, kind).await?;
    let mut freq = cfg.freq_mhz;
    match &mut transport {
        AnyTransport::E22(e) => {
            e.set_lbt(resolved.is_some_and(|r| r.plan.lbt.is_some()))
                .await?;
        }
        AnyTransport::Meshtastic(m) => {
            freq = apply_meshtastic(m, resolved, &ant).await?;
        }
        AnyTransport::Loopback(_) => {}
    }
    let mut primary = lane(
        &cfg,
        transport,
        resolved,
        freq,
        Path::new(&cfg.ledger_path),
        now,
    )?;
    // The E22 receives on its configured channel: park it on the lane's.
    if let (AnyTransport::E22(e), Some(f)) =
        (&mut primary.transport, primary.governor.current_channel())
    {
        e.set_rx_channel(f).await?;
    }

    let mut mesh_state = MeshtasticState {
        enabled: cfg.meshtastic.enabled || kind == TransportKind::Meshtastic,
        connected: kind == TransportKind::Meshtastic,
        port: cfg.meshtastic.port.clone(),
        region_preset: preset,
    };
    let bridge = if cfg.meshtastic.enabled && kind != TransportKind::Meshtastic {
        match Meshtastic::open(&cfg).await {
            Ok(mut m) => {
                let f = apply_meshtastic(&mut m, resolved, &ant).await?;
                mesh_state.connected = true;
                let path =
                    PathBuf::from(&cfg.ledger_path).with_file_name("airtime-meshtastic.jsonl");
                Some(lane(
                    &cfg,
                    AnyTransport::Meshtastic(Box::new(m)),
                    resolved,
                    f,
                    &path,
                    now,
                )?)
            }
            Err(e) => {
                tracing::warn!(error = %e, "lora_meshtastic_bridge_unavailable");
                None
            }
        }
    } else {
        None
    };

    let gov = &primary.governor;
    let budget_s = gov.current_channel().and_then(|f| {
        let plan = gov.plan()?;
        let d = plan.sub_band_for(f, gov.setup().modem.bw_khz)?.duty?;
        Some(d.budget_us() as f64 / 1e6)
    });
    let interval_s = beacon_interval_s(cfg.beacon_interval_s, primary.cycle_charge_us()?, budget_s);
    let lane_state = LaneState {
        transport: kind.as_str(),
        module_band: primary.transport.caps().band.clone(),
        resolved,
        node_id: cfg.node_id,
        rx_frames: 0,
        rx_crc_errors: 0,
        meshtastic: mesh_state,
    };
    Ok(Some(Built {
        primary,
        bridge,
        lane_state,
        interval_s,
        origin,
        origin_mono_ns,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_lora::regions::by_code;

    #[test]
    fn meshtastic_slots_are_accounted_inside_the_plan() {
        // EU_868 sits in sub-band P.
        let f = meshtastic_channel(by_code("EU").unwrap(), "EU_868").unwrap();
        assert!((f - 869.525).abs() < 1e-9);
        // MY_919 is pinned to the 923-924 slot the plan allows.
        let f = meshtastic_channel(by_code("MY").unwrap(), "MY_919").unwrap();
        assert!((f - 923.5).abs() < 1e-9);
        assert!(meshtastic_channel(by_code("IN").unwrap(), "NONE").is_none());
    }

    #[test]
    fn the_meshtastic_node_transmits_only_when_a_step_fits_inside_the_plan() {
        let mesh = ModuleCaps::meshtastic_863_928();
        let dipole = Antenna {
            gain_dbi: 2.15,
            cable_loss_db: 0.0,
        };
        let india = by_code("IN").unwrap();
        // 25 mW ERP with a dipole: 13 dBm conducted.
        assert_eq!(
            meshtastic_slot(india, india.meshtastic, &mesh, 125, &dipole),
            Some(("IN", 866.0, 13))
        );
        // A 20 dBi antenna leaves no 1 dB step under the clamp: transmit off.
        let big = Antenna {
            gain_dbi: 20.0,
            cable_loss_db: 0.0,
        };
        assert_eq!(
            meshtastic_slot(india, india.meshtastic, &mesh, 125, &big),
            None
        );
        // Japan needs a listen the host cannot take through the node.
        let jp = by_code("JP").unwrap();
        assert_eq!(
            meshtastic_slot(jp, jp.meshtastic, &mesh, 125, &dipole),
            None
        );
        // No preset for the plan.
        let vn = by_code("VN").unwrap();
        assert_eq!(
            meshtastic_slot(vn, vn.meshtastic, &mesh, 125, &dipole),
            None
        );
    }
}
