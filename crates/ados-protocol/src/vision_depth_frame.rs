//! Metric depth frames (`vision.depth.v2`): the vision engine's monocular
//! depth, scaled to metres with ranging hits, with a per-pixel uncertainty,
//! for consumers that build a 3D map from it.
//!
//! **Plane.** [`FRAME_COLS`] x [`FRAME_ROWS`] pixels covering the whole camera
//! frame, row-major from the top-left. Pixel `(i, j)` has its centre at image
//! coordinates `(i + 0.5, j + 0.5)` in plane pixels, and a camera point
//! `(X, Y, Z)` (x right, y down, z along the optical axis) lands at
//! `(fx * X / Z + cx, fy * Y / Z + cy)`. The plane squeezes the frame, so
//! `fx` and `fy` differ for any frame that is not 4:3. `pitch_down_deg` is the
//! optical axis below the vehicle's nose (the camera mount the engine was
//! configured with).
//!
//! **Values.** `depth_mm` is the optical-axis depth `Z` in millimetres, `0`
//! where there is none (sky, or beyond [`u16::MAX`] mm). `sigma_mm` is its one
//! standard deviation in millimetres, saturating at [`u16::MAX`]. Both planes
//! travel as little-endian `u16` bytes.
//!
//! **Fit.** The model ranks depth but does not measure it; `fit` says how the
//! frame was scaled: `1 / Z = scale * x + shift` on the model's values `x`
//! normalised per frame (the frame's 5th percentile value to 0, its 95th to
//! 1), from `inliers` ranging hits that left a median range residual of
//! `residual_m`. `held_ms` is `0` for a fit made on this frame and the age of
//! the last good fit when this frame had too few hits for one (its `sigma_mm`
//! is then inflated with that age). A frame without any fit is not published.
//!
//! **Size.** The record must fit one plugin event (64 KiB of msgpack, the
//! event bus's payload cap): two `u16` planes of 128 x 96 are 48 KiB, where
//! the model's own 256 x 192 class resolution would be 192 KiB. The planes are
//! therefore published at 128 x 96.
//!
//! **Wire.** One msgpack map per frame: `v, camera_id, stamp_ns, w, h, fx, fy,
//! cx, cy, pitch_down_deg, depth_mm: bin, sigma_mm: bin, fit: {scale, shift,
//! inliers, residual_m, held_ms}`. `stamp_ns` is the capture time of the frame
//! on the host monotonic clock. Frames come at most every
//! [`MIN_INTERVAL_MS`] per camera.

use rmpv::Value;
use thiserror::Error;

/// The record version this module writes and reads.
pub const VISION_DEPTH_FRAME_VERSION: u64 = 2;

/// The event topic the frames are published on.
pub const VISION_DEPTH_FRAME_TOPIC: &str = "vision.depth.v2";

/// The capability a plugin needs (beside `event.subscribe`) to read
/// [`VISION_DEPTH_FRAME_TOPIC`]; only the host publishes the topic.
pub const VISION_DEPTH_READ_CAP: &str = "vision.depth.read";

/// Published plane width, pixels.
pub const FRAME_COLS: u16 = 128;
/// Published plane height, pixels.
pub const FRAME_ROWS: u16 = 96;

/// The largest encoded record: one plugin event payload.
pub const FRAME_MAX_BYTES: usize = 64 * 1024;

/// The shortest gap between two frames from one camera (5 Hz).
pub const MIN_INTERVAL_MS: u64 = 200;

/// How a frame was scaled to metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthFit {
    /// `1 / Z = scale * x + shift` on normalised model values.
    pub scale: f32,
    pub shift: f32,
    /// Ranging hits the fit agreed with.
    pub inliers: u32,
    /// Median `|Z_fit - Z_measured|` over the inliers, metres.
    pub residual_m: f32,
    /// `0` for a fit made on this frame, else the held fit's age.
    pub held_ms: u32,
}

/// One camera's metric depth frame.
#[derive(Debug, Clone, PartialEq)]
pub struct DepthFrame {
    pub camera_id: String,
    pub stamp_ns: u64,
    pub w: u16,
    pub h: u16,
    pub fx: f32,
    pub fy: f32,
    pub cx: f32,
    pub cy: f32,
    pub pitch_down_deg: f32,
    /// `w * h` depths, millimetres, `0` unknown.
    pub depth_mm: Vec<u16>,
    /// `w * h` standard deviations, millimetres.
    pub sigma_mm: Vec<u16>,
    pub fit: DepthFit,
}

/// A frame that does not decode.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DepthFrameError {
    #[error("depth frame is not valid msgpack: {0}")]
    Msgpack(String),
    #[error("depth frame field {field}: {reason}")]
    Field { field: &'static str, reason: String },
    #[error("depth frame version {0} is not {VISION_DEPTH_FRAME_VERSION}")]
    Version(u64),
}

fn plane_bytes(plane: &[u16]) -> Value {
    Value::Binary(plane.iter().flat_map(|v| v.to_le_bytes()).collect())
}

impl DepthFrame {
    /// Encode as a msgpack map (no length prefix).
    pub fn to_msgpack(&self) -> Vec<u8> {
        let f = &self.fit;
        let value = map(vec![
            ("v", Value::from(VISION_DEPTH_FRAME_VERSION)),
            ("camera_id", Value::from(self.camera_id.as_str())),
            ("stamp_ns", Value::from(self.stamp_ns)),
            ("w", Value::from(self.w)),
            ("h", Value::from(self.h)),
            ("fx", Value::F32(self.fx)),
            ("fy", Value::F32(self.fy)),
            ("cx", Value::F32(self.cx)),
            ("cy", Value::F32(self.cy)),
            ("pitch_down_deg", Value::F32(self.pitch_down_deg)),
            ("depth_mm", plane_bytes(&self.depth_mm)),
            ("sigma_mm", plane_bytes(&self.sigma_mm)),
            (
                "fit",
                map(vec![
                    ("scale", Value::F32(f.scale)),
                    ("shift", Value::F32(f.shift)),
                    ("inliers", Value::from(f.inliers)),
                    ("residual_m", Value::F32(f.residual_m)),
                    ("held_ms", Value::from(f.held_ms)),
                ]),
            ),
        ]);
        let mut out = Vec::with_capacity(self.depth_mm.len() * 4 + 256);
        // Writing to a Vec cannot fail.
        rmpv::encode::write_value(&mut out, &value).expect("msgpack into a Vec");
        out
    }

    /// Decode a msgpack map written by [`Self::to_msgpack`].
    pub fn from_msgpack(bytes: &[u8]) -> Result<Self, DepthFrameError> {
        let value = rmpv::decode::read_value(&mut &bytes[..])
            .map_err(|e| DepthFrameError::Msgpack(e.to_string()))?;
        Self::from_value(&value)
    }

    /// Decode an already-parsed record (the map [`Self::to_msgpack`] writes).
    /// The intrinsics must be finite and positive and the mount pitch finite.
    pub fn from_value(value: &Value) -> Result<Self, DepthFrameError> {
        let v = uint(value, "v")?;
        if v != VISION_DEPTH_FRAME_VERSION {
            return Err(DepthFrameError::Version(v));
        }
        let w = u16::try_from(uint(value, "w")?).map_err(|_| field_err("w", "out of range"))?;
        let h = u16::try_from(uint(value, "h")?).map_err(|_| field_err("h", "out of range"))?;
        let pixels = usize::from(w) * usize::from(h);
        let fit = get(value, "fit")?;
        let pitch_down_deg = num(value, "pitch_down_deg")?;
        if !pitch_down_deg.is_finite() {
            return Err(field_err("pitch_down_deg", "not finite"));
        }
        Ok(Self {
            camera_id: get(value, "camera_id")?
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| field_err("camera_id", "not a string"))?,
            stamp_ns: uint(value, "stamp_ns")?,
            w,
            h,
            fx: positive(value, "fx")?,
            fy: positive(value, "fy")?,
            cx: positive(value, "cx")?,
            cy: positive(value, "cy")?,
            pitch_down_deg,
            depth_mm: plane(value, "depth_mm", pixels)?,
            sigma_mm: plane(value, "sigma_mm", pixels)?,
            fit: DepthFit {
                scale: num(fit, "scale")?,
                shift: num(fit, "shift")?,
                inliers: u32::try_from(uint(fit, "inliers")?)
                    .map_err(|_| field_err("inliers", "out of range"))?,
                residual_m: num(fit, "residual_m")?,
                held_ms: u32::try_from(uint(fit, "held_ms")?)
                    .map_err(|_| field_err("held_ms", "out of range"))?,
            },
        })
    }
}

/// A finite, strictly positive number.
fn positive(v: &Value, key: &'static str) -> Result<f32, DepthFrameError> {
    let x = num(v, key)?;
    if x > 0.0 && x.is_finite() {
        Ok(x)
    } else {
        Err(field_err(key, format!("{x} is not finite and positive")))
    }
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(
        pairs
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect(),
    )
}

fn get<'a>(v: &'a Value, key: &'static str) -> Result<&'a Value, DepthFrameError> {
    v.as_map()
        .ok_or_else(|| field_err(key, "record is not a map"))?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
        .ok_or_else(|| field_err(key, "missing"))
}

fn uint(v: &Value, key: &'static str) -> Result<u64, DepthFrameError> {
    get(v, key)?
        .as_u64()
        .ok_or_else(|| field_err(key, "not an unsigned integer"))
}

fn num(v: &Value, key: &'static str) -> Result<f32, DepthFrameError> {
    get(v, key)?
        .as_f64()
        .map(|f| f as f32)
        .ok_or_else(|| field_err(key, "not a number"))
}

fn plane(v: &Value, key: &'static str, pixels: usize) -> Result<Vec<u16>, DepthFrameError> {
    let Value::Binary(b) = get(v, key)? else {
        return Err(field_err(key, "not binary"));
    };
    if b.len() != pixels * 2 {
        return Err(field_err(
            key,
            format!("{} bytes for {pixels} pixels", b.len()),
        ));
    }
    let (pairs, _) = b.as_chunks::<2>();
    Ok(pairs.iter().map(|c| u16::from_le_bytes(*c)).collect())
}

fn field_err(field: &'static str, reason: impl ToString) -> DepthFrameError {
    DepthFrameError::Field {
        field,
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u16, h: u16) -> DepthFrame {
        let n = usize::from(w) * usize::from(h);
        DepthFrame {
            camera_id: "c".repeat(64),
            stamp_ns: u64::MAX,
            w,
            h,
            fx: 110.9,
            fy: 83.2,
            cx: f32::from(w) / 2.0,
            cy: f32::from(h) / 2.0,
            pitch_down_deg: 15.0,
            depth_mm: (0..n).map(|i| i as u16 | 0x8000).collect(),
            sigma_mm: vec![u16::MAX; n],
            fit: DepthFit {
                scale: 0.21,
                shift: 0.03,
                inliers: u32::MAX,
                residual_m: 0.4,
                held_ms: u32::MAX,
            },
        }
    }

    #[test]
    fn a_frame_round_trips() {
        let f = frame(FRAME_COLS, FRAME_ROWS);
        assert_eq!(DepthFrame::from_msgpack(&f.to_msgpack()).unwrap(), f);
    }

    #[test]
    fn the_largest_frame_fits_one_event_and_the_model_resolution_would_not() {
        let len = frame(FRAME_COLS, FRAME_ROWS).to_msgpack().len();
        assert!(len <= FRAME_MAX_BYTES, "{len} bytes");
        assert!(frame(256, 192).to_msgpack().len() > FRAME_MAX_BYTES);
    }

    #[test]
    fn a_short_plane_and_a_wrong_version_are_refused() {
        let mut short = frame(4, 3);
        short.sigma_mm.pop();
        assert!(matches!(
            DepthFrame::from_msgpack(&short.to_msgpack()),
            Err(DepthFrameError::Field {
                field: "sigma_mm",
                ..
            })
        ));
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &map(vec![("v", Value::from(1))])).unwrap();
        assert_eq!(
            DepthFrame::from_msgpack(&bytes),
            Err(DepthFrameError::Version(1))
        );
    }

    #[test]
    fn non_finite_or_non_positive_intrinsics_are_refused() {
        for bad in [f32::NAN, f32::INFINITY, 0.0, -1.0] {
            for field in ["fx", "fy", "cx", "cy"] {
                let mut f = frame(4, 3);
                match field {
                    "fx" => f.fx = bad,
                    "fy" => f.fy = bad,
                    "cx" => f.cx = bad,
                    _ => f.cy = bad,
                }
                let err = DepthFrame::from_msgpack(&f.to_msgpack()).unwrap_err();
                assert!(
                    matches!(err, DepthFrameError::Field { field: got, .. } if got == field),
                    "{field} = {bad}: {err}"
                );
            }
        }
        let mut f = frame(4, 3);
        f.pitch_down_deg = f32::NAN;
        assert!(DepthFrame::from_msgpack(&f.to_msgpack()).is_err());
        // The parsed-value path is the same decoder.
        let ok = frame(4, 3);
        let value = rmpv::decode::read_value(&mut &ok.to_msgpack()[..]).unwrap();
        assert_eq!(DepthFrame::from_value(&value).unwrap(), ok);
    }
}
