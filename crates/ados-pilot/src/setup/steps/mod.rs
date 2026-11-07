//! One file per step (the three one-command calibrations share
//! `simple_cal`; `radio` and `rc_ados_switch` share the RC capture).

pub mod accel;
pub mod ados;
pub mod autotune;
pub mod battery;
pub mod compass;
pub mod detect;
pub mod first_hover;
pub mod frame;
pub mod initial_params;
pub mod notch;
pub mod outputs;
pub mod plane_air;
pub mod preflight;
pub mod radio;
pub mod simple_cal;
pub mod verify;

use super::step::Step;

/// A fresh step for `id`, `None` for an unknown id.
pub fn make(id: &str) -> Option<Box<dyn Step>> {
    Some(match id {
        "detect" => Box::new(detect::Detect::default()),
        "frame" => Box::new(frame::Frame::default()),
        "accel" => Box::new(accel::Accel::default()),
        "level" => Box::new(simple_cal::SimpleCal::new(simple_cal::Kind::Level)),
        "gyro" => Box::new(simple_cal::SimpleCal::new(simple_cal::Kind::Gyro)),
        "airspeed" => Box::new(simple_cal::SimpleCal::new(simple_cal::Kind::Airspeed)),
        "compass" => Box::new(compass::Compass::default()),
        "radio" => Box::new(radio::Radio::default()),
        "rc_ados_switch" => Box::new(radio::RcAdosSwitch::default()),
        "outputs" => Box::new(outputs::Outputs::default()),
        "battery" => Box::new(battery::Battery::default()),
        "initial_params" => Box::new(initial_params::InitialParams::default()),
        "notch_static" => Box::new(notch::Notch::new(false)),
        "notch_air" => Box::new(notch::Notch::new(true)),
        "ados_profile" => Box::new(ados::AdosProfile::default()),
        "ados_intro" => Box::new(ados::AdosIntro::default()),
        "preflight" => Box::new(preflight::Preflight::default()),
        "first_hover" => Box::new(first_hover::FirstHover::default()),
        "first_flight" => Box::new(plane_air::PlaneAir::new(plane_air::Kind::FirstFlight)),
        "trim_air" => Box::new(plane_air::PlaneAir::new(plane_air::Kind::TrimAir)),
        "airspeed_cal" => Box::new(plane_air::PlaneAir::new(plane_air::Kind::AirspeedCal)),
        "tecs_measure" => Box::new(plane_air::PlaneAir::new(plane_air::Kind::TecsMeasure)),
        "autotune" => Box::new(autotune::Autotune::default()),
        "verify" => Box::new(verify::Verify::default()),
        _ => return None,
    })
}
