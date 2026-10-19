use super::*;
use crate::fixtures::{self, unframe};
use crate::ubx;

const S: u64 = 1_000_000_000;

fn cap(model: Option<&str>) -> Capability {
    Capability {
        writable: true,
        model: model.map(str::to_string),
        port: RxPort::Uart1,
    }
}

fn survey() -> BaseMode {
    BaseMode::Survey {
        min_dur_s: 300,
        accuracy_m: 2.0,
    }
}

fn svin(dur: u32, acc: u32, valid: bool, active: bool) -> NavSvin {
    let f = fixtures::nav_svin(
        dur,
        [133_565_088, 607_215_978, 142_255_026],
        acc,
        valid,
        active,
    );
    ubx::nav_svin(&unframe(&f).2).unwrap()
}

#[test]
fn survey_runs_to_fixed_on_a_valid_report() {
    let mut b = Base::new(300, 2.0);
    let bytes = b.apply(survey(), &cap(Some("u-blox ZED-F9P")), 0).unwrap();
    assert_eq!(bytes, Some(valset::survey_in(RxPort::Uart1, 300, 2.0)));
    assert_eq!(b.state(), BaseState::Surveying);
    let v = b.view();
    assert_eq!((v.elapsed_s, v.accuracy_m, v.fixed), (0, None, None));

    // Not yet started: no progress claimed.
    b.on_svin(&svin(0, 0, false, false));
    assert_eq!(b.view().accuracy_m, None);
    b.on_svin(&svin(120, 31_000, false, true));
    let v = b.view();
    assert_eq!(v.state, BaseState::Surveying);
    assert_eq!(v.elapsed_s, 120);
    assert!((v.accuracy_m.unwrap() - 3.1).abs() < 1e-9);
    assert!(b.active());
    // Confirmed: the timeout no longer applies.
    b.tick(60 * S);
    assert_eq!(b.state(), BaseState::Surveying);

    b.on_svin(&svin(310, 19_000, true, false));
    let v = b.view();
    assert_eq!(v.state, BaseState::Fixed);
    assert_eq!(v.mode, "survey");
    let p = v.fixed.unwrap();
    assert!((p.lat_deg - 12.9716).abs() < 1e-6);
    assert!((p.lon_deg - 77.5946).abs() < 1e-6);
    assert!((p.h_ellipsoid_m - 920.4).abs() < 0.02);
}

#[test]
fn survey_fails_on_an_unsupported_model() {
    let mut b = Base::new(300, 2.0);
    let e = b
        .apply(survey(), &cap(Some("u-blox NEO-M10")), 0)
        .unwrap_err();
    assert_eq!(e.0, E_UNSUPPORTED);
    assert_eq!(b.state(), BaseState::Failed);
    assert!(b.reason().unwrap().contains("NEO-M10"));
    assert!(!b.active());

    // Model learned from MON-VER after the command.
    let mut b = Base::new(300, 2.0);
    b.apply(survey(), &cap(None), 0).unwrap();
    b.on_model("u-blox NEO-M9N");
    assert_eq!(b.state(), BaseState::Failed);
    // A later survey report does not resurrect it.
    b.on_svin(&svin(5, 1, false, true));
    assert_eq!(b.state(), BaseState::Failed);
    assert!(survey_capable("u-blox ZED-X20P"));
}

#[test]
fn only_listed_base_modules_are_survey_capable() {
    for m in [
        "u-blox ZED-F9P",
        "ZED-F9P",
        "u-blox NEO-F9P",
        "u-blox zed-x20p",
    ] {
        assert!(survey_capable(m), "{m}");
    }
    for m in [
        "u-blox ZED-F9R",
        "u-blox ZED-F9T",
        "u-blox ZED-F9H",
        "u-blox NEO-F9P-15B-xx",
        "u-blox ZED-F9P2",
        "u-blox hw 00190000",
        "F9",
        "X20",
        "",
    ] {
        assert!(!survey_capable(m), "{m}");
    }
    // A rover module learned from MON-VER fails a requested survey.
    let mut b = Base::new(300, 2.0);
    b.apply(survey(), &cap(None), 0).unwrap();
    b.on_model("u-blox ZED-F9R");
    assert_eq!(b.state(), BaseState::Failed);
}

#[test]
fn survey_fails_without_a_report() {
    let mut b = Base::new(300, 2.0);
    b.apply(survey(), &cap(None), 0).unwrap();
    b.tick(CONFIRM_TIMEOUT_NS);
    assert_eq!(b.state(), BaseState::Surveying);
    b.tick(CONFIRM_TIMEOUT_NS + 1);
    assert_eq!(b.state(), BaseState::Failed);
    assert!(b.reason().unwrap().contains("NAV-SVIN"));
}

#[test]
fn hub_receiver_cannot_be_a_base() {
    let mut b = Base::new(300, 2.0);
    let hub = Capability::default();
    let e = b.apply(survey(), &hub, 0).unwrap_err();
    assert_eq!(e.0, "E_GNSS_UNSUPPORTED");
    assert_eq!(b.state(), BaseState::Failed);
    // Off needs nothing written.
    assert_eq!(b.apply(BaseMode::Off, &hub, 0), Ok(None));
    assert_eq!(b.state(), BaseState::Off);
}

#[test]
fn fixed_waits_for_the_ack() {
    let pos = Llh {
        lat_deg: 12.9716,
        lon_deg: 77.5946,
        h_ellipsoid_m: 920.4,
    };
    let mut b = Base::new(300, 2.0);
    b.apply(BaseMode::Fixed(pos), &cap(Some("u-blox ZED-F9P")), 0)
        .unwrap();
    assert_eq!(b.state(), BaseState::Off);
    assert!(b.view().fixed.is_none());
    b.on_valset_ack(true);
    let v = b.view();
    assert_eq!((v.mode, v.state), ("fixed", BaseState::Fixed));
    assert_eq!(v.fixed, Some(pos));

    let mut b = Base::new(300, 2.0);
    b.apply(BaseMode::Fixed(pos), &cap(None), 0).unwrap();
    b.on_valset_ack(false);
    assert_eq!(b.state(), BaseState::Failed);

    let mut b = Base::new(300, 2.0);
    b.apply(BaseMode::Fixed(pos), &cap(None), 0).unwrap();
    b.tick(CONFIRM_TIMEOUT_NS + 1);
    assert_eq!(b.state(), BaseState::Failed);
}

#[test]
fn off_leaves_base_mode() {
    let mut b = Base::new(300, 2.0);
    b.apply(survey(), &cap(None), 0).unwrap();
    let bytes = b.apply(BaseMode::Off, &cap(None), S).unwrap();
    assert_eq!(bytes, Some(valset::off(RxPort::Uart1)));
    let v = b.view();
    assert_eq!((v.mode, v.state, v.fixed), ("off", BaseState::Off, None));
    assert!(!b.active());
    b.tick(100 * S);
    assert_eq!(b.state(), BaseState::Off);
    // The survey settings stay as the defaults for the next request.
    assert_eq!(b.defaults(), (300, 2.0));
}

#[test]
fn ecef_conversion_of_reference_points() {
    let p = ecef_to_llh([6_378_137.0, 0.0, 0.0]);
    assert!(p.lat_deg.abs() < 1e-9 && p.lon_deg.abs() < 1e-9 && p.h_ellipsoid_m.abs() < 1e-6);
    let pole = ecef_to_llh([0.0, 0.0, 6_356_752.314_245]);
    assert!((pole.lat_deg - 90.0).abs() < 1e-9 && pole.h_ellipsoid_m.abs() < 1e-3);
}
