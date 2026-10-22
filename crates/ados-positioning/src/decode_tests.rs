//! UBX / NMEA decode, fix mapping and the receiver solution.

use crate::fixtures::{self, unframe, Pvt};
use crate::nmea::{self, Sentence};
use crate::receiver::{fix_from_pvt, Fix, ReceiverState, Rtk, STALE_NS};
use crate::ubx;

fn payload(frame: &[u8]) -> Vec<u8> {
    unframe(frame).2
}

#[test]
fn nav_pvt_decodes_position_time_and_quality() {
    let f = fixtures::nav_pvt(Pvt {
        utc: Some((2026, 10, 19, 10, 15, 0, 250_000_000)),
        ..Pvt::default()
    });
    let (class, id, p) = unframe(&f);
    assert_eq!((class, id), (ubx::CLS_NAV, ubx::NAV_PVT));
    let pvt = ubx::nav_pvt(&p).unwrap();
    assert_eq!(pvt.fix_type, 3);
    assert!(pvt.gnss_fix_ok);
    assert_eq!(pvt.num_sv, 14);
    assert!((pvt.lat_deg - 12.9716).abs() < 1e-9);
    assert!((pvt.lon_deg - 77.5946).abs() < 1e-9);
    assert!((pvt.h_ellipsoid_m - 920.4).abs() < 1e-9);
    assert!((pvt.h_msl_m - 1006.1).abs() < 1e-9);
    assert_eq!(pvt.utc_ns, Some(1_792_404_900_250_000_000));
    assert_eq!(ubx::days_from_civil(1970, 1, 1), 0);
    assert_eq!(ubx::days_from_civil(2026, 10, 19), 20_745);
    // Time not valid: no UTC.
    let p = payload(&fixtures::nav_pvt(Pvt::default()));
    assert_eq!(ubx::nav_pvt(&p).unwrap().utc_ns, None);
    assert!(ubx::nav_pvt(&p[..50]).is_none());
}

#[test]
fn nav_dop_reads_hdop() {
    let p = payload(&fixtures::nav_dop(85));
    assert!((ubx::nav_dop_hdop(&p).unwrap() - 0.85).abs() < 1e-9);
}

#[test]
fn nav_svin_reads_progress_and_mean() {
    let p = payload(&fixtures::nav_svin(
        120,
        [133_565_088, 607_215_978, 142_255_026],
        31_000,
        false,
        true,
    ));
    let s = ubx::nav_svin(&p).unwrap();
    assert_eq!(s.dur_s, 120);
    assert!((s.mean_acc_m - 3.1).abs() < 1e-9);
    assert!(s.active && !s.valid);
    assert!((s.mean_ecef_m[0] - 1_335_650.88).abs() < 1e-6);
}

#[test]
fn nav_relposned_reads_heading_and_flags() {
    let flags = (1 << 0) | (1 << 2) | (1 << 5) | (1 << 8);
    let p = payload(&fixtures::nav_relposned(9_012_345, 25_000, 102, flags));
    let r = ubx::nav_relposned(&p).unwrap();
    assert!((r.heading_deg - 90.12345).abs() < 1e-9);
    assert!((r.acc_heading_deg - 0.25).abs() < 1e-9);
    assert!((r.length_m - 1.02).abs() < 1e-9);
    assert!(r.gnss_fix_ok && r.rel_pos_valid && r.is_moving && r.heading_valid);
    let r = ubx::nav_relposned(&payload(&fixtures::nav_relposned(0, 0, 0, 1))).unwrap();
    assert!(!r.heading_valid && !r.rel_pos_valid);
}

#[test]
fn tim_tp_gives_the_next_pulse_utc() {
    let half_ms = 1u32 << 31;
    let gps = payload(&fixtures::tim_tp(123_456_000, half_ms, 2389, 0x00));
    assert_eq!(ubx::tim_tp_utc_ns(&gps), Some(1_760_955_438_000_500_000));
    let utc = payload(&fixtures::tim_tp(123_456_000, half_ms, 2389, 0x01));
    assert_eq!(ubx::tim_tp_utc_ns(&utc), Some(1_760_955_456_000_500_000));
}

#[test]
fn mon_ver_reports_the_module() {
    let p = payload(&fixtures::mon_ver(
        "00190000",
        &["PROTVER=27.12", "MOD=ZED-F9P"],
    ));
    assert_eq!(ubx::mon_ver_model(&p).as_deref(), Some("u-blox ZED-F9P"));
    let p = payload(&fixtures::mon_ver("000A0000", &[]));
    assert_eq!(
        ubx::mon_ver_model(&p).as_deref(),
        Some("u-blox hw 000A0000")
    );
}

#[test]
fn fix_mapping_covers_every_solution() {
    let pvt = |fix_type: u8, flags: u8| {
        ubx::nav_pvt(&payload(&fixtures::nav_pvt(Pvt {
            fix_type,
            flags,
            ..Pvt::default()
        })))
        .unwrap()
    };
    let cases = [
        (0, 0x01, Fix::None),
        (3, 0x00, Fix::None), // gnssFixOK clear
        (1, 0x01, Fix::None), // dead reckoning only
        (5, 0x01, Fix::None), // time only
        (2, 0x01, Fix::TwoD),
        (3, 0x01, Fix::ThreeD),
        (4, 0x01, Fix::ThreeD),
        (3, 0x03, Fix::Dgps),
        (3, 0x43, Fix::RtkFloat),
        (3, 0x83, Fix::RtkFixed),
    ];
    for (t, f, want) in cases {
        assert_eq!(fix_from_pvt(&pvt(t, f)), want, "fixType {t} flags {f:#x}");
    }
    assert_eq!(Fix::RtkFloat.rtk(), Rtk::Float);
    assert_eq!(Fix::RtkFixed.rtk(), Rtk::Fixed);
    assert_eq!(Fix::Dgps.rtk(), Rtk::None);
    assert_eq!(serde_json::to_value(Fix::TwoD).unwrap(), "2d");
    assert_eq!(serde_json::to_value(Fix::RtkFixed).unwrap(), "rtk_fixed");
}

#[test]
fn nmea_gga_and_rmc_decode() {
    let gga = "$GNGGA,123519,4807.038,N,01131.000,W,4,12,0.7,545.4,M,46.9,M,,";
    let Some(Sentence::Gga(g)) = nmea::parse(gga) else {
        panic!("gga");
    };
    assert_eq!(g.fix, Fix::RtkFixed);
    assert_eq!(g.sats, 12);
    assert_eq!(g.hdop, Some(0.7));
    assert!((g.lat_deg.unwrap() - 48.1173).abs() < 1e-9);
    assert!((g.lon_deg.unwrap() + 11.516_666_666).abs() < 1e-6);
    assert!((g.h_ellipsoid_m.unwrap() - 592.3).abs() < 1e-9);
    let rmc = "$GPRMC,123519,A,4807.038,S,01131.000,E,022.4,084.4,230394,003.1,W,D";
    let Some(Sentence::Rmc(r)) = nmea::parse(rmc) else {
        panic!("rmc");
    };
    assert_eq!(r.fix, Fix::Dgps);
    assert!(r.lat_deg.unwrap() < 0.0);
    let void = "$GPRMC,123519,V,,,,,,,230394,,,N";
    assert!(matches!(nmea::parse(void), Some(Sentence::Rmc(r)) if r.fix == Fix::None));
    assert!(nmea::parse("$GPGSV,1,1,00").is_none());
}

#[test]
fn solution_prefers_fresh_ubx_and_nav_dop() {
    let mut rx = ReceiverState::new(None);
    let pvt = ubx::nav_pvt(&payload(&fixtures::nav_pvt(Pvt::default()))).unwrap();
    let gga = "$GNGGA,1,4807.038,N,01131.000,E,1,05,2.0,1.0,M,1.0,M,,";
    rx.on_nmea(1, nmea::parse(gga).unwrap());
    rx.on_pvt(1, pvt);
    let s = rx.solution(2);
    assert_eq!((s.fix, s.sats), (Fix::ThreeD, 14));
    // No NAV-DOP yet: hdop is unknown (PDOP is never reported as hdop).
    assert_eq!(s.hdop, None);
    rx.on_dop(2, 0.8);
    assert_eq!(rx.solution(3).hdop, Some(0.8));
    // UBX stale, GGA re-sent: NMEA fallback.
    let later = 1 + STALE_NS + 1;
    rx.on_nmea(later, nmea::parse(gga).unwrap());
    let s = rx.solution(later);
    assert_eq!((s.fix, s.sats, s.hdop), (Fix::ThreeD, 5, Some(2.0)));
    // Everything stale: no fix, nulls.
    let s = rx.solution(later + STALE_NS + 1);
    assert_eq!(s.fix, Fix::None);
    assert!(s.lat_deg.is_none() && s.hdop.is_none());
}
