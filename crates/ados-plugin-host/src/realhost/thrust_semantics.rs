//! Whether the flight controller reads a `SET_ATTITUDE_TARGET` thrust as thrust.
//!
//! `flight.rate_setpoint.send` puts a plugin's collective thrust on the wire
//! unchanged, so the host refuses a thrust-commanding setpoint the autopilot
//! would read as something else. ArduCopter reads the field as thrust only
//! while `GUID_OPTIONS` bit 3 is set (a climb rate otherwise, 0.5 holding
//! height) and ArduSub always reads a climb rate; ArduPlane (QuadPlane
//! included), ArduRover and PX4 read it as thrust or throttle. The facts come
//! from the MAVLink router's parameter cache: the parameters it persists and,
//! beside them, the identity of the flight controller they belong to. The
//! router writes a changed parameter at once outside its boot sweep, so the
//! `PARAM_VALUE` echo of a `GUID_OPTIONS` write reaches the verdict here.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use ados_protocol::flight_modes::ArduPilotFirmware;
use ados_protocol::mavlink::ardupilotmega::{MavAutopilot, MavType};
use ados_protocol::mavlink::GUID_OPTIONS_THRUST;
use serde::Deserialize;

/// Where the MAVLink router persists its parameter cache. Mirrors the router's
/// `param_cache::DEFAULT_PARAMS_PATH`; the two are separate binaries with no
/// shared crate, so the constant is duplicated as `ados-control` does.
pub(super) const PARAMS_PATH: &str = "/var/lib/ados/params.json";

/// The longest a verdict is reused while neither file appears to change, so a
/// rewrite the file stamp cannot tell apart still lands within this bound.
const REREAD_AFTER: Duration = Duration::from_secs(1);

/// How the flight controller reads a commanded thrust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ThrustReading {
    /// Collective thrust (or throttle): the plugin's value is sent unchanged.
    Thrust,
    /// A climb rate: ArduCopter with `GUID_OPTIONS` bit 3 clear, or ArduSub.
    ClimbRate,
    /// Not known: no flight controller recorded yet, or an ArduCopter whose
    /// `GUID_OPTIONS` the cache does not hold (the parameter sweep has not
    /// reached it, or the firmware predates the option).
    Unknown,
}

impl ThrustReading {
    /// The `(reason, detail)` a thrust-commanding setpoint is refused with, or
    /// `None` when it may be sent.
    pub(super) fn refusal(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Thrust => None,
            Self::ClimbRate => Some((
                "thrust_is_climb_rate",
                "the flight controller reads SET_ATTITUDE_TARGET thrust as a climb rate; \
                 on ArduCopter set GUID_OPTIONS bit 3 (value 8) so it reads thrust",
            )),
            Self::Unknown => Some((
                "thrust_semantics_unknown",
                "the flight controller's GUID_OPTIONS has not been read, so whether it \
                 reads SET_ATTITUDE_TARGET thrust as thrust is unknown",
            )),
        }
    }
}

/// The flight-controller identity the router records beside its cache.
#[derive(Debug, Clone, Copy, Deserialize)]
struct FcRecord {
    autopilot: u8,
    vehicle_type: u8,
}

#[derive(Debug, Deserialize)]
struct Entry {
    value: f64,
}

/// The two cached parameters the verdict reads; serde skips every other name.
#[derive(Debug, Default, Deserialize)]
struct Params {
    #[serde(rename = "GUID_OPTIONS")]
    guid_options: Option<Entry>,
    /// Present on every ArduPlane, QuadPlane included, and never on ArduCopter,
    /// so it tells a QuadPlane reporting a multirotor type from a copter.
    #[serde(rename = "Q_ENABLE")]
    q_enable: Option<Entry>,
}

fn classify(fc: Option<FcRecord>, params: &Params) -> ThrustReading {
    let Some(fc) = fc else {
        return ThrustReading::Unknown;
    };
    if fc.autopilot != MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA as u8 {
        return ThrustReading::Thrust;
    }
    if fc.vehicle_type == MavType::MAV_TYPE_SUBMARINE as u8 {
        return ThrustReading::ClimbRate;
    }
    let copter = ArduPilotFirmware::from_mav_type(i64::from(fc.vehicle_type))
        == Some(ArduPilotFirmware::Copter);
    if !copter || params.q_enable.is_some() {
        return ThrustReading::Thrust;
    }
    match &params.guid_options {
        Some(e) if (e.value as u32) & GUID_OPTIONS_THRUST != 0 => ThrustReading::Thrust,
        Some(_) => ThrustReading::ClimbRate,
        None => ThrustReading::Unknown,
    }
}

/// What identifies one version of a file: its inode, modification time and
/// length (`None` when absent). The router replaces the cache by rename, so a
/// rewrite normally changes the inode as well as the time.
type Stamp = Option<(u64, SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some((m.ino(), m.modified().ok()?, m.len()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The thrust verdict for the flight controller the router's cache describes,
/// re-derived only when the cache or its identity file changes.
pub(super) struct ThrustSemantics {
    params_path: PathBuf,
    fc_path: PathBuf,
    cached: Mutex<Option<([Stamp; 2], Instant, ThrustReading)>>,
}

impl ThrustSemantics {
    /// A verdict source reading the cache at `params_path` and the identity the
    /// router writes beside it (`<stem>.fc.json`).
    pub(super) fn new(params_path: PathBuf) -> Self {
        Self {
            fc_path: params_path.with_extension("fc.json"),
            params_path,
            cached: Mutex::new(None),
        }
    }

    /// How the flight controller reads a commanded thrust right now.
    pub(super) fn reading(&self) -> ThrustReading {
        let stamps = [stamp(&self.params_path), stamp(&self.fc_path)];
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((seen, at, reading)) = *cached {
            if seen == stamps && at.elapsed() < REREAD_AFTER {
                return reading;
            }
        }
        let params = read_json(&self.params_path).unwrap_or_default();
        let reading = classify(read_json(&self.fc_path), &params);
        *cached = Some((stamps, Instant::now(), reading));
        reading
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARDUPILOT: u8 = 3;
    const PX4: u8 = 12;
    const QUADROTOR: u8 = 2;
    const FIXED_WING: u8 = 1;
    const GROUND_ROVER: u8 = 10;
    const SUBMARINE: u8 = 12;

    fn fc(autopilot: u8, vehicle_type: u8) -> Option<FcRecord> {
        Some(FcRecord {
            autopilot,
            vehicle_type,
        })
    }

    fn params(guid_options: Option<f64>, q_enable: bool) -> Params {
        Params {
            guid_options: guid_options.map(|value| Entry { value }),
            q_enable: q_enable.then_some(Entry { value: 1.0 }),
        }
    }

    #[test]
    fn arducopter_reads_thrust_only_with_guid_options_bit_3() {
        let copter = fc(ARDUPILOT, QUADROTOR);
        assert_eq!(
            classify(copter, &params(Some(8.0), false)),
            ThrustReading::Thrust
        );
        // Bit 3 ORed into other operator bits still reads thrust.
        assert_eq!(
            classify(copter, &params(Some(9.0), false)),
            ThrustReading::Thrust
        );
        assert_eq!(
            classify(copter, &params(Some(0.0), false)),
            ThrustReading::ClimbRate
        );
        assert_eq!(
            classify(copter, &params(Some(7.0), false)),
            ThrustReading::ClimbRate
        );
        assert_eq!(
            classify(copter, &params(None, false)),
            ThrustReading::Unknown
        );
    }

    #[test]
    fn other_firmware_reads_thrust_and_ardusub_a_climb_rate() {
        let none = params(None, false);
        assert_eq!(classify(fc(PX4, QUADROTOR), &none), ThrustReading::Thrust);
        assert_eq!(
            classify(fc(ARDUPILOT, FIXED_WING), &none),
            ThrustReading::Thrust
        );
        assert_eq!(
            classify(fc(ARDUPILOT, GROUND_ROVER), &none),
            ThrustReading::Thrust
        );
        // A QuadPlane may report a multirotor type; Q_ENABLE marks it a plane.
        assert_eq!(
            classify(fc(ARDUPILOT, QUADROTOR), &params(None, true)),
            ThrustReading::Thrust
        );
        assert_eq!(
            classify(fc(ARDUPILOT, SUBMARINE), &params(Some(8.0), false)),
            ThrustReading::ClimbRate
        );
        assert_eq!(classify(None, &none), ThrustReading::Unknown);
    }

    #[test]
    fn the_verdict_follows_a_rewritten_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        let semantics = ThrustSemantics::new(path.clone());
        assert_eq!(semantics.reading(), ThrustReading::Unknown);

        std::fs::write(
            dir.path().join("params.fc.json"),
            r#"{"system_id":1,"autopilot":3,"vehicle_type":2}"#,
        )
        .unwrap();
        let write = |guid: f64| {
            let tmp = path.with_extension("json.tmp");
            let body = format!(
                r#"{{"GUID_OPTIONS":{{"value":{guid:.1},"param_type":6,"last_updated":1.0}},"WPNAV_SPEED":{{"value":500.0,"param_type":9,"last_updated":1.0}}}}"#
            );
            std::fs::write(&tmp, body).unwrap();
            std::fs::rename(&tmp, &path).unwrap();
        };
        write(0.0);
        assert_eq!(semantics.reading(), ThrustReading::ClimbRate);
        // Bit 3 plus bit 10: a body of a different length, so the rewrite is
        // told apart even inside one coarse timestamp tick.
        write(1032.0);
        assert_eq!(semantics.reading(), ThrustReading::Thrust);
    }
}
