//! A surface's "fly here" (C1 `goto`).

use ados_protocol::pilot::{ControlMode, Owner};
use serde_json::Value;

use super::ActionRes;
use crate::core::Pilot;
use crate::flows::LaunchStep;

/// Metres per degree of latitude (spherical earth).
const M_PER_DEG: f64 = 111_319.49;

impl Pilot {
    /// A surface's "fly here": `{lat, lon, alt_m}` with the altitude above
    /// home, converted to local NED from the FC's home position.
    pub(super) fn surface_goto(&mut self, args: Option<&Value>, now_ms: u64) -> ActionRes {
        let num = |k: &str| args.and_then(|a| a.get(k)).and_then(Value::as_f64);
        let (Some(lat), Some(lon), Some(alt)) = (num("lat"), num("lon"), num("alt_m")) else {
            return Err("bad_args".into());
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) || !alt.is_finite() {
            return Err("bad_args".into());
        }
        if !self.fc.armed {
            return Err("not_armed".into());
        }
        if self.landed() {
            return Err("landed".into());
        }
        if self.owner() == Owner::Tuning {
            return Err("tuning".into());
        }
        // Outside `ados` the FC flies: a surface repositions it directly.
        if self.control_mode() != ControlMode::Ados {
            return Err("control_mode".into());
        }
        // The FC is flying a failsafe RTL or land of its own: the operator
        // changes mode first (Take control, or the Mode sheet).
        if self.fc_failsafe {
            return Err("failsafe".into());
        }
        // A take-off still climbing would replace the goto with its own
        // after-takeoff action at the top of the climb, so it waits.
        if self
            .launch
            .as_ref()
            .is_some_and(|l| l.step != LaunchStep::Done)
        {
            return Err("launching".into());
        }
        let (Some([hlat, hlon]), Some(home)) = (self.fc.home_global, self.fc.home_local) else {
            return Err("no_home".into());
        };
        let mut dlon = lon - hlon;
        if dlon > 180.0 {
            dlon -= 360.0;
        } else if dlon < -180.0 {
            dlon += 360.0;
        }
        let n = (lat - hlat) * M_PER_DEG;
        let e = dlon * M_PER_DEG * hlat.to_radians().cos();
        let alt = alt.clamp(1.0, 120.0);
        let target = [home[0] + n as f32, home[1] + e as f32, home[2] - alt as f32];
        self.start_goto(target, now_ms);
        Ok(())
    }
}
