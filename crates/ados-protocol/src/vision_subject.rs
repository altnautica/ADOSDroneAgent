//! The designated subject's world state (`vision.subject.v1`): where the
//! person the operator designated is, how sure the estimate is, how they are
//! moving and where they will be over the next few seconds.
//!
//! **Frame.** Positions and velocities are local NED metres (the flight
//! controller's local frame, the one `LOCAL_POSITION_NED` reports).
//! `pos_ned` is the subject's ground-contact point (the feet), not the box
//! centre. `cov` is the 3 x 3 position covariance, row-major, m².
//!
//! **Motion.** `mode_probs` are the interacting-multiple-model weights of the
//! three motion models, in [`MODE_NAMES`] order: constant velocity, a
//! coordinated turn and standing still. `predictions` are the expected
//! positions at the [`PREDICTION_HORIZONS_S`] offsets after `stamp_ns`, each
//! with its own covariance, kept out of space a map reports as occupied when
//! the producer has one.
//!
//! **Sources.** `sources` names the cues that fed the estimate within the
//! last second: `box` (the box height against the subject's height),
//! `depth` (metric depth inside the box), `ground` (the line of sight to the
//! feet against the ground height under the subject) and `beacon` (the
//! subject's own GNSS position). `fix_age_ms` is the time since the last
//! measurement of any kind, a bearing-only sighting included; a subject
//! hidden for a second carries an empty list.
//!
//! **Wire.** One msgpack map per update: `v, camera_id, id, stamp_ns,
//! fix_age_ms, pos_ned, cov, vel_ned, mode_probs, height_m, predictions:
//! [{dt_s, pos_ned, cov}], sources: [str]`, every float an `f32`. `id` is the
//! subject's published `track_id` on `vision.detection`. `stamp_ns` is on the
//! frame clock the tracker steps on: a simulated frame's sim time, else the
//! frame's capture time on the host monotonic clock.

use rmpv::Value;
use thiserror::Error;

/// The record version this module writes and reads, from the contract
/// registry.
pub const VISION_SUBJECT_VERSION: u16 = crate::contracts::contract_version("vision.subject.v1")
    .expect("vision.subject.v1 is registered in contracts.toml");

/// The topic the subject state is published on.
pub const VISION_SUBJECT_TOPIC: &str = "vision.subject.v1";

/// The prediction offsets after `stamp_ns`, seconds.
pub const PREDICTION_HORIZONS_S: [f32; 5] = [0.5, 1.0, 1.5, 2.0, 3.0];

/// The motion models `mode_probs` weighs, in order.
pub const MODE_NAMES: [&str; 3] = ["cv", "ct", "stop"];

/// The largest encoded record (a full one is well under a kilobyte).
pub const SUBJECT_MAX_BYTES: usize = 4 * 1024;

/// The most predictions a record may carry.
const MAX_PREDICTIONS: usize = 8;

/// Which range cues the latest fix fused.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubjectSources {
    /// The box height against the subject's height.
    pub box_height: bool,
    /// Metric depth inside the box.
    pub depth: bool,
    /// The line of sight to the feet against the ground height.
    pub ground: bool,
    /// The subject's own GNSS position.
    pub beacon: bool,
}

impl SubjectSources {
    const NAMES: [&'static str; 4] = ["box", "depth", "ground", "beacon"];

    fn flags(&self) -> [bool; 4] {
        [self.box_height, self.depth, self.ground, self.beacon]
    }

    /// Whether any cue fused.
    pub fn any(&self) -> bool {
        self.flags().contains(&true)
    }
}

/// One predicted position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubjectPrediction {
    /// Seconds after the state's `stamp_ns`.
    pub dt_s: f32,
    pub pos_ned: [f32; 3],
    /// Row-major 3 x 3 covariance, m².
    pub cov: [f32; 9],
}

/// The designated subject's state at one instant.
#[derive(Debug, Clone, PartialEq)]
pub struct SubjectState {
    pub camera_id: String,
    /// The subject's published track id.
    pub id: u32,
    pub stamp_ns: u64,
    /// Time since the last fix, ms; `0` when this update fixed.
    pub fix_age_ms: u32,
    /// Ground-contact point, NED metres.
    pub pos_ned: [f32; 3],
    /// Row-major 3 x 3 position covariance, m².
    pub cov: [f32; 9],
    pub vel_ned: [f32; 3],
    /// Constant velocity, coordinated turn, standing still.
    pub mode_probs: [f32; 3],
    /// The subject's estimated height, metres.
    pub height_m: f32,
    pub predictions: Vec<SubjectPrediction>,
    pub sources: SubjectSources,
}

/// A record that does not decode.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SubjectStateError {
    #[error("subject state is not valid msgpack: {0}")]
    Msgpack(String),
    #[error("subject state field {field}: {reason}")]
    Field { field: &'static str, reason: String },
    #[error("subject state version {0} is not {VISION_SUBJECT_VERSION}")]
    Version(u64),
}

impl SubjectState {
    /// Encode as a msgpack map (no length prefix).
    pub fn to_msgpack(&self) -> Vec<u8> {
        let predictions = self
            .predictions
            .iter()
            .map(|p| {
                map(vec![
                    ("dt_s", Value::F32(p.dt_s)),
                    ("pos_ned", floats(&p.pos_ned)),
                    ("cov", floats(&p.cov)),
                ])
            })
            .collect();
        let sources = SubjectSources::NAMES
            .iter()
            .zip(self.sources.flags())
            .filter(|(_, on)| *on)
            .map(|(name, _)| Value::from(*name))
            .collect();
        let value = map(vec![
            ("v", Value::from(VISION_SUBJECT_VERSION)),
            ("camera_id", Value::from(self.camera_id.as_str())),
            ("id", Value::from(self.id)),
            ("stamp_ns", Value::from(self.stamp_ns)),
            ("fix_age_ms", Value::from(self.fix_age_ms)),
            ("pos_ned", floats(&self.pos_ned)),
            ("cov", floats(&self.cov)),
            ("vel_ned", floats(&self.vel_ned)),
            ("mode_probs", floats(&self.mode_probs)),
            ("height_m", Value::F32(self.height_m)),
            ("predictions", Value::Array(predictions)),
            ("sources", Value::Array(sources)),
        ]);
        let mut out = Vec::with_capacity(768);
        // Writing to a Vec cannot fail.
        rmpv::encode::write_value(&mut out, &value).expect("msgpack into a Vec");
        out
    }

    /// Decode a msgpack map written by [`Self::to_msgpack`].
    pub fn from_msgpack(bytes: &[u8]) -> Result<Self, SubjectStateError> {
        let value = rmpv::decode::read_value(&mut &bytes[..])
            .map_err(|e| SubjectStateError::Msgpack(e.to_string()))?;
        let v = uint(&value, "v")?;
        if v != u64::from(VISION_SUBJECT_VERSION) {
            return Err(SubjectStateError::Version(v));
        }
        let Value::Array(preds) = get(&value, "predictions")? else {
            return Err(field_err("predictions", "not an array"));
        };
        if preds.len() > MAX_PREDICTIONS {
            return Err(field_err("predictions", format!("{} entries", preds.len())));
        }
        let predictions = preds
            .iter()
            .map(|p| {
                Ok(SubjectPrediction {
                    dt_s: num(p, "dt_s")?,
                    pos_ned: array(p, "pos_ned")?,
                    cov: array(p, "cov")?,
                })
            })
            .collect::<Result<_, SubjectStateError>>()?;
        let Value::Array(names) = get(&value, "sources")? else {
            return Err(field_err("sources", "not an array"));
        };
        let mut flags = [false; 4];
        for name in names {
            let i = SubjectSources::NAMES
                .iter()
                .position(|n| Some(*n) == name.as_str())
                .ok_or_else(|| field_err("sources", format!("unknown source {name}")))?;
            flags[i] = true;
        }
        Ok(Self {
            camera_id: get(&value, "camera_id")?
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| field_err("camera_id", "not a string"))?,
            id: u32::try_from(uint(&value, "id")?).map_err(|_| field_err("id", "out of range"))?,
            stamp_ns: uint(&value, "stamp_ns")?,
            fix_age_ms: u32::try_from(uint(&value, "fix_age_ms")?)
                .map_err(|_| field_err("fix_age_ms", "out of range"))?,
            pos_ned: array(&value, "pos_ned")?,
            cov: array(&value, "cov")?,
            vel_ned: array(&value, "vel_ned")?,
            mode_probs: array(&value, "mode_probs")?,
            height_m: num(&value, "height_m")?,
            predictions,
            sources: SubjectSources {
                box_height: flags[0],
                depth: flags[1],
                ground: flags[2],
                beacon: flags[3],
            },
        })
    }
}

fn floats(values: &[f32]) -> Value {
    Value::Array(values.iter().map(|&f| Value::F32(f)).collect())
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(
        pairs
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect(),
    )
}

fn get<'a>(v: &'a Value, key: &'static str) -> Result<&'a Value, SubjectStateError> {
    v.as_map()
        .ok_or_else(|| field_err(key, "record is not a map"))?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
        .ok_or_else(|| field_err(key, "missing"))
}

fn uint(v: &Value, key: &'static str) -> Result<u64, SubjectStateError> {
    get(v, key)?
        .as_u64()
        .ok_or_else(|| field_err(key, "not an unsigned integer"))
}

fn num(v: &Value, key: &'static str) -> Result<f32, SubjectStateError> {
    get(v, key)?
        .as_f64()
        .map(|f| f as f32)
        .ok_or_else(|| field_err(key, "not a number"))
}

fn array<const N: usize>(v: &Value, key: &'static str) -> Result<[f32; N], SubjectStateError> {
    let Value::Array(items) = get(v, key)? else {
        return Err(field_err(key, "not an array"));
    };
    if items.len() != N {
        return Err(field_err(key, format!("{} values, not {N}", items.len())));
    }
    let mut out = [0.0; N];
    for (o, item) in out.iter_mut().zip(items) {
        *o = item
            .as_f64()
            .ok_or_else(|| field_err(key, "not a number"))? as f32;
    }
    Ok(out)
}

fn field_err(field: &'static str, reason: impl ToString) -> SubjectStateError {
    SubjectStateError::Field {
        field,
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> SubjectState {
        SubjectState {
            camera_id: "main".into(),
            id: 7,
            stamp_ns: u64::MAX,
            fix_age_ms: 120,
            pos_ned: [8.0, -2.5, 0.1],
            cov: [0.2, 0.01, 0.0, 0.01, 0.3, 0.0, 0.0, 0.0, 0.05],
            vel_ned: [1.4, 0.0, 0.0],
            mode_probs: [0.7, 0.2, 0.1],
            height_m: 1.74,
            predictions: PREDICTION_HORIZONS_S
                .iter()
                .map(|&dt_s| SubjectPrediction {
                    dt_s,
                    pos_ned: [8.0 + 1.4 * dt_s, -2.5, 0.1],
                    cov: [dt_s; 9],
                })
                .collect(),
            sources: SubjectSources {
                box_height: true,
                depth: true,
                ground: false,
                beacon: true,
            },
        }
    }

    #[test]
    fn a_state_round_trips_and_fits_its_bound() {
        let s = state();
        let bytes = s.to_msgpack();
        assert!(bytes.len() <= SUBJECT_MAX_BYTES, "{} bytes", bytes.len());
        assert_eq!(SubjectState::from_msgpack(&bytes).unwrap(), s);
    }

    #[test]
    fn a_coasting_state_names_no_source() {
        let s = SubjectState {
            sources: SubjectSources::default(),
            ..state()
        };
        assert!(!s.sources.any());
        let back = SubjectState::from_msgpack(&s.to_msgpack()).unwrap();
        assert_eq!(back.sources, SubjectSources::default());
    }

    /// A valid record with one field replaced.
    fn with(field: &str, replacement: Value) -> Vec<u8> {
        let mut value = rmpv::decode::read_value(&mut &state().to_msgpack()[..]).unwrap();
        if let Value::Map(entries) = &mut value {
            for (k, v) in entries.iter_mut() {
                if k.as_str() == Some(field) {
                    *v = replacement.clone();
                }
            }
        }
        let mut out = Vec::new();
        rmpv::encode::write_value(&mut out, &value).unwrap();
        out
    }

    #[test]
    fn a_wrong_version_a_short_array_and_an_unknown_source_are_refused() {
        assert_eq!(
            SubjectState::from_msgpack(&with("v", Value::from(9))),
            Err(SubjectStateError::Version(9))
        );
        assert!(matches!(
            SubjectState::from_msgpack(&with("cov", floats(&[1.0; 4]))),
            Err(SubjectStateError::Field { field: "cov", .. })
        ));
        assert!(matches!(
            SubjectState::from_msgpack(&with("sources", Value::Array(vec![Value::from("sonar")]))),
            Err(SubjectStateError::Field {
                field: "sources",
                ..
            })
        ));
    }
}
