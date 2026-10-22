use super::power::Antenna;
use super::*;
use crate::airtime::Modem;
use crate::regions::{by_code, RegionSource, Resolved};

const S: i64 = 1_000_000_000;

/// SF7 / 125 kHz / 4-5 with a 759-symbol preamble: a 4-byte payload is
/// exactly 800 ms on air (1.024 ms x 781.25 symbols), so 45 frames are
/// exactly the 36 s an hour the India plan allows.
const MODEM_800MS: Modem = Modem {
    sf: 7,
    bw_khz: 125,
    cr: 1,
    preamble: 759,
    crc: true,
    implicit_header: false,
};

fn at(sec: i64) -> Now {
    Now {
        mono_ns: sec * S,
        utc_ms: 1_760_000_000_000 + sec * 1000,
        utc_trusted: false,
    }
}

fn governor(
    dir: &std::path::Path,
    boot: &str,
    code: Option<&str>,
    caps: ModuleCaps,
    modem: Modem,
    freq_mhz: Option<f64>,
    start: Now,
) -> Governor {
    let resolved = code.map(|c| Resolved {
        plan: by_code(c).unwrap(),
        source: RegionSource::LoraRegion,
    });
    let window = max_window_s(resolved.map(|r| r.plan));
    let (ledger, _) = Ledger::open(&dir.join("airtime.jsonl"), boot, start, window).unwrap();
    Governor::new(
        Setup {
            resolved,
            caps,
            modem,
            airtime_factor: 1,
            freq_mhz,
            erp_mw: None,
            antenna: Antenna {
                gain_dbi: 2.15,
                cable_loss_db: 0.0,
            },
            node_id: 7,
        },
        ledger,
    )
}

fn send(g: &mut Governor, len: usize, ch: Option<f64>, now: Now) -> Result<Grant, Refusal> {
    let grant = g.admit(len, ch, now)?;
    g.commit(&grant, now).unwrap();
    g.note_sent();
    Ok(grant)
}

#[test]
fn the_800ms_test_frame_is_exact() {
    assert_eq!(crate::airtime::time_on_air_us(&MODEM_800MS, 4), 800_000);
}

#[test]
fn india_allows_exactly_36_s_per_rolling_hour_then_duty_exhausted() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = governor(
        dir.path(),
        "b",
        Some("IN"),
        ModuleCaps::any(),
        MODEM_800MS,
        None,
        at(0),
    );
    for i in 0..45 {
        let grant = send(&mut g, 4, None, at(i)).unwrap();
        assert_eq!(grant.channel_mhz, 866.125);
        assert_eq!(grant.sub_band, "865.0-867.0");
    }
    assert_eq!(g.admit(4, None, at(45)), Err(Refusal::DutyExhausted));
    let st = g.status(4, at(45));
    assert_eq!(st.blocked_reason, Some("duty_exhausted"));
    assert!(!st.tx_allowed);
    assert_eq!(st.tx["refused_by_rule"]["duty_cycle"], 1);
    let duty = &st.governor["rules"][1];
    assert_eq!(duty["kind"], "duty_cycle");
    assert_eq!(duty["airtime_ms_window"], 36_000);
    assert_eq!(duty["budget_ms_window"], 36_000);
    assert_eq!(duty["exhausted"], true);
    // The first frame (t = 0) ages out of the window at t = 3600 s.
    assert_eq!(g.next_tx_allowed_ms(4, at(45)), 3_555_000);
    let freed = g.status(4, at(3600));
    assert!(freed.tx_allowed);
    assert_eq!(freed.blocked_reason, None);
    assert!(g.admit(4, None, at(3600)).is_ok());
}

#[test]
fn eu_sub_bands_are_accounted_separately() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = governor(
        dir.path(),
        "b",
        Some("EU"),
        ModuleCaps::any(),
        MODEM_800MS,
        None,
        at(0),
    );
    // M (868.0-868.6, 1 %, 25 mW ERP): 45 frames.
    for i in 0..45 {
        let grant = send(&mut g, 4, Some(868.1), at(i)).unwrap();
        assert_eq!(grant.power_dbm, 13);
    }
    assert_eq!(g.admit(4, Some(868.1), at(46)), Err(Refusal::DutyExhausted));
    // P (869.4-869.65, 10 %, 500 mW ERP) keeps admitting up to 360 s.
    for i in 0..450 {
        let grant = send(&mut g, 4, Some(869.525), at(50 + i)).unwrap();
        assert_eq!(grant.sub_band, "869.4-869.65");
        assert_eq!(grant.power_dbm, 26);
    }
    assert_eq!(
        g.admit(4, Some(869.525), at(600)),
        Err(Refusal::DutyExhausted)
    );
}

#[test]
fn us_dwell_400ms_per_20s_on_one_channel() {
    let dir = tempfile::tempdir().unwrap();
    let modem = Modem::SF9_BW125;
    let mut g = governor(
        dir.path(),
        "b",
        Some("US"),
        ModuleCaps::e22_900(),
        modem,
        Some(902.125),
        at(0),
    );
    // 22 bytes = 205.824 ms: one fits in 400 ms, a second does not.
    send(&mut g, 22, None, at(0)).unwrap();
    assert_eq!(g.admit(22, None, at(1)), Err(Refusal::DwellTime));
    assert_eq!(g.status(22, at(1)).blocked_reason, Some("dwell_time"));
    // Ten-byte frames (144.384 ms): two fit, the third does not.
    let dir2 = tempfile::tempdir().unwrap();
    let mut g2 = governor(
        dir2.path(),
        "b",
        Some("US"),
        ModuleCaps::e22_900(),
        modem,
        Some(902.125),
        at(0),
    );
    send(&mut g2, 10, None, at(0)).unwrap();
    send(&mut g2, 10, None, at(1)).unwrap();
    assert_eq!(g2.admit(10, None, at(2)), Err(Refusal::DwellTime));
    // The first ages out of the 20 s period.
    assert!(g2.admit(10, None, at(20)).is_ok());
}

#[test]
fn us_hopping_spreads_frames_and_never_hits_dwell() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = governor(
        dir.path(),
        "b",
        Some("US"),
        ModuleCaps::e22_900(),
        Modem::SF9_BW125,
        None,
        at(0),
    );
    let mut used = std::collections::HashSet::new();
    for i in 0..60 {
        let grant = send(&mut g, 22, None, at(i)).unwrap();
        used.insert((grant.channel_mhz * 1000.0) as i64);
        // 30 dBm conducted capped by the 8 dBm/3 kHz PSD at 125 kHz: the
        // 22 dBm module step is the highest that fits.
        assert_eq!(grant.power_dbm, 22);
    }
    assert_eq!(used.len(), 26);
}

#[test]
fn region_unset_blocks_transmit() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = governor(
        dir.path(),
        "b",
        None,
        ModuleCaps::e22_900(),
        Modem::SF9_BW125,
        None,
        at(0),
    );
    assert_eq!(g.admit(22, None, at(0)), Err(Refusal::RegionUnset));
    let st = g.status(22, at(0));
    assert!(!st.tx_allowed);
    assert_eq!(st.blocked_reason, Some("region_unset"));
    assert_eq!(g.next_tx_allowed_ms(22, at(0)), 0);
    assert_eq!(st.governor["rules"].as_array().unwrap().len(), 0);
}

#[test]
fn cn470_on_the_900_mhz_module_is_a_band_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = governor(
        dir.path(),
        "b",
        Some("CN"),
        ModuleCaps::e22_900(),
        Modem::SF9_BW125,
        None,
        at(0),
    );
    assert_eq!(g.admit(22, None, at(0)), Err(Refusal::ModuleBandMismatch));
    assert_eq!(
        g.status(22, at(0)).blocked_reason,
        Some("module_band_mismatch")
    );
    // On a module that tunes 470 MHz the research-only row still blocks.
    let dir2 = tempfile::tempdir().unwrap();
    let mut g2 = governor(
        dir2.path(),
        "b",
        Some("CN"),
        ModuleCaps::e22_400(),
        Modem::SF9_BW125,
        None,
        at(0),
    );
    assert_eq!(g2.admit(22, None, at(0)), Err(Refusal::RegionUnset));
}

#[test]
fn power_over_limit_when_no_module_step_fits() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = governor(
        dir.path(),
        "b",
        Some("EU433"),
        ModuleCaps::e22_400(),
        Modem::SF9_BW125,
        None,
        at(0),
    );
    g.setup.antenna.gain_dbi = 9.0;
    assert_eq!(g.admit(22, None, at(0)), Err(Refusal::PowerOverLimit));
    assert_eq!(g.status(22, at(0)).blocked_reason, Some("power_over_limit"));
}

#[test]
fn the_airtime_spend_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = governor(
        dir.path(),
        "boot-1",
        Some("IN"),
        ModuleCaps::any(),
        MODEM_800MS,
        None,
        at(0),
    );
    for i in 0..37 {
        send(&mut g, 4, None, at(i)).unwrap();
    }
    drop(g);
    // Reboot: a new boot id, monotonic restarted, no trusted wall clock.
    // 29.6 s were spent, so 6.4 s (eight frames) remain.
    let mut g = governor(
        dir.path(),
        "boot-2",
        Some("IN"),
        ModuleCaps::any(),
        MODEM_800MS,
        None,
        at(5),
    );
    for i in 0..8 {
        send(&mut g, 4, None, at(10 + i)).unwrap();
    }
    assert_eq!(g.admit(4, None, at(20)), Err(Refusal::DutyExhausted));
}

#[test]
fn e22_frames_are_charged_twice_the_formula() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = governor(
        dir.path(),
        "b",
        Some("IN"),
        ModuleCaps::e22_900(),
        Modem::SF9_BW125,
        None,
        at(0),
    );
    g.setup.airtime_factor = 2;
    assert_eq!(g.charge_us(22), 411_648);
}

#[test]
fn japan_through_a_meshtastic_node_is_a_band_mismatch() {
    // The node cannot hand the host an RSSI listen, and JP requires one.
    let dir = tempfile::tempdir().unwrap();
    let mut g = governor(
        dir.path(),
        "b",
        Some("JP"),
        ModuleCaps::meshtastic_863_928(),
        Modem::SF9_BW125,
        None,
        at(0),
    );
    assert_eq!(g.admit(41, None, at(0)), Err(Refusal::ModuleBandMismatch));
    assert_eq!(
        g.status(41, at(0)).blocked_reason,
        Some("module_band_mismatch")
    );
}

#[test]
fn a_frame_refused_after_its_charge_is_not_counted_as_sent() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = governor(
        dir.path(),
        "b",
        Some("IN"),
        ModuleCaps::any(),
        MODEM_800MS,
        None,
        at(0),
    );
    // Charged, then the re-listen right before the write finds the channel busy.
    let grant = g.admit(4, None, at(0)).unwrap();
    g.commit(&grant, at(0)).unwrap();
    g.refuse(Refusal::LbtBusy, at(0));
    let st = g.status(4, at(1));
    assert_eq!(st.tx["frames"], 0);
    assert_eq!(st.tx["refused"], 1);
    // The charge still stands (over-counting, never under-counting).
    assert_eq!(st.governor["rules"][1]["airtime_ms_window"], 800);
    send(&mut g, 4, None, at(2)).unwrap();
    assert_eq!(g.status(4, at(3)).tx["frames"], 1);
}
