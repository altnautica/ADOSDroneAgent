//! The vision engine's depth outputs: the per-camera depth grid and the
//! landing-zone candidates, as the engine broadcasts them on
//! `vision-depth.sock` and the control plane serves them as the `depth` and
//! `surfaces` nav kinds.
//!
//! **Grid.** A camera's image is split into [`GRID_COLS`] x [`GRID_ROWS`]
//! cells, row-major from the top-left. Each cell holds one byte: `0` is
//! unknown, `1..=255` a range on a log scale from [`GRID_MIN_M`] to
//! [`GRID_MAX_M`] ([`encode_range_m`], [`decode_range_code`]). The `measured`
//! bitmask has one bit per cell (cell `i` is bit `i % 8` of byte `i / 8`, LSB
//! first): set where a ranging sensor (the planar LiDAR or the downward
//! rangefinder) hit the cell, so the value is measured rather than inferred.
//!
//! **Scale.** A monocular model ranks depth but does not measure it. When the
//! engine could fit the model to ranging hits, the grid is in metres and
//! `scale_source` is `lidar`. Otherwise it is `none` and each cell is a
//! relative band only: [`UNSCALED_NEAR_M`] for the nearest band,
//! [`UNSCALED_MID_M`] for the next, [`UNSCALED_FAR_M`] for the rest.
//!
//! **Wire** (`vision-depth.sock`): one record per frame, a 4-byte big-endian
//! length prefix ([`crate::frame`]) and a msgpack map, `kind` naming the
//! record:
//!
//! | kind | fields |
//! |---|---|
//! | `depth` | `v, camera_id, mono_ns, scale_source, cols, rows, near: bin, measured: bin` |
//! | `surfaces` | `v, camera_id, mono_ns, zones: [{pts: [[x, y]..], score, slope_deg}]` |
//!
//! `mono_ns` is the capture time of the frame on the host monotonic clock;
//! zone points are normalised to the frame, `0..1`, y down.

use rmpv::Value;
use thiserror::Error;

/// The record version this module writes and reads.
pub const VISION_DEPTH_VERSION: u64 = 1;

/// Grid columns.
pub const GRID_COLS: usize = 32;
/// Grid rows.
pub const GRID_ROWS: usize = 18;
/// Cells in one grid.
pub const GRID_CELLS: usize = GRID_COLS * GRID_ROWS;
/// Bytes in one `measured` bitmask.
pub const GRID_MASK_BYTES: usize = GRID_CELLS.div_ceil(8);

/// The nearest range a cell code can hold, metres.
pub const GRID_MIN_M: f32 = 0.2;
/// The farthest range a cell code can hold, metres.
pub const GRID_MAX_M: f32 = 60.0;

/// The range an unscaled grid writes for its nearest band.
pub const UNSCALED_NEAR_M: f32 = 1.5;
/// The range an unscaled grid writes for its middle band.
pub const UNSCALED_MID_M: f32 = 3.5;
/// The range an unscaled grid writes for everything else.
pub const UNSCALED_FAR_M: f32 = 20.0;

/// The cell code for a range in metres, clamped to the code's span.
pub fn encode_range_m(d: f32) -> u8 {
    if !d.is_finite() {
        return if d > 0.0 { 255 } else { 0 };
    }
    let d = d.clamp(GRID_MIN_M, GRID_MAX_M);
    let span = (GRID_MAX_M / GRID_MIN_M).ln();
    let v = 1.0 + 254.0 * (d / GRID_MIN_M).ln() / span;
    v.round().clamp(1.0, 255.0) as u8
}

/// The range a cell code stands for, `None` for the unknown code `0`.
pub fn decode_range_code(v: u8) -> Option<f32> {
    if v == 0 {
        return None;
    }
    let span = (GRID_MAX_M / GRID_MIN_M).ln();
    Some(GRID_MIN_M * ((f32::from(v) - 1.0) * span / 254.0).exp())
}

/// How a grid's ranges were scaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScaleSource {
    /// Fitted to ranging hits: metres.
    Lidar,
    /// Not fitted: relative bands only.
    #[default]
    None,
}

impl ScaleSource {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ScaleSource::Lidar => "lidar",
            ScaleSource::None => "none",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "lidar" => Some(ScaleSource::Lidar),
            "none" => Some(ScaleSource::None),
            _ => None,
        }
    }
}

/// One camera's depth grid.
#[derive(Debug, Clone, PartialEq)]
pub struct DepthGrid {
    pub camera_id: String,
    pub mono_ns: u64,
    pub scale_source: ScaleSource,
    pub cols: u16,
    pub rows: u16,
    /// One code per cell, row-major, `cols * rows` bytes.
    pub near: Vec<u8>,
    /// One bit per cell, LSB first.
    pub measured: Vec<u8>,
}

/// One landing-zone candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceZone {
    /// Outline in normalised frame coordinates, `0..1`, y down.
    pub pts: Vec<[f32; 2]>,
    /// `0..1`, larger flat areas score higher.
    pub score: f32,
    /// The fitted plane's tilt from level, degrees.
    pub slope_deg: f32,
}

/// One camera's landing-zone candidates.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfacesFrame {
    pub camera_id: String,
    pub mono_ns: u64,
    pub zones: Vec<SurfaceZone>,
}

/// One `vision-depth.sock` record.
#[derive(Debug, Clone, PartialEq)]
pub enum DepthRecord {
    Depth(DepthGrid),
    Surfaces(SurfacesFrame),
}

/// A record that does not decode.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DepthRecordError {
    #[error("depth record is not valid msgpack: {0}")]
    Msgpack(String),
    #[error("depth record field {field}: {reason}")]
    Field { field: &'static str, reason: String },
    #[error("depth record version {0} is not {VISION_DEPTH_VERSION}")]
    Version(u64),
}

impl DepthRecord {
    /// The camera the record is for.
    pub fn camera_id(&self) -> &str {
        match self {
            DepthRecord::Depth(g) => &g.camera_id,
            DepthRecord::Surfaces(s) => &s.camera_id,
        }
    }

    /// Encode as a msgpack map (no length prefix).
    pub fn to_msgpack(&self) -> Vec<u8> {
        let value = match self {
            DepthRecord::Depth(g) => map(vec![
                ("kind", Value::from("depth")),
                ("v", Value::from(VISION_DEPTH_VERSION)),
                ("camera_id", Value::from(g.camera_id.as_str())),
                ("mono_ns", Value::from(g.mono_ns)),
                ("scale_source", Value::from(g.scale_source.as_str())),
                ("cols", Value::from(g.cols)),
                ("rows", Value::from(g.rows)),
                ("near", Value::Binary(g.near.clone())),
                ("measured", Value::Binary(g.measured.clone())),
            ]),
            DepthRecord::Surfaces(s) => map(vec![
                ("kind", Value::from("surfaces")),
                ("v", Value::from(VISION_DEPTH_VERSION)),
                ("camera_id", Value::from(s.camera_id.as_str())),
                ("mono_ns", Value::from(s.mono_ns)),
                (
                    "zones",
                    Value::Array(
                        s.zones
                            .iter()
                            .map(|z| {
                                map(vec![
                                    (
                                        "pts",
                                        Value::Array(
                                            z.pts
                                                .iter()
                                                .map(|p| {
                                                    Value::Array(vec![
                                                        Value::F32(p[0]),
                                                        Value::F32(p[1]),
                                                    ])
                                                })
                                                .collect(),
                                        ),
                                    ),
                                    ("score", Value::F32(z.score)),
                                    ("slope_deg", Value::F32(z.slope_deg)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]),
        };
        let mut out = Vec::new();
        // Writing to a Vec cannot fail.
        rmpv::encode::write_value(&mut out, &value).expect("msgpack into a Vec");
        out
    }

    /// Decode a msgpack map written by [`Self::to_msgpack`].
    pub fn from_msgpack(bytes: &[u8]) -> Result<Self, DepthRecordError> {
        let value = rmpv::decode::read_value(&mut &bytes[..])
            .map_err(|e| DepthRecordError::Msgpack(e.to_string()))?;
        let v = uint(&value, "v")?;
        if v != VISION_DEPTH_VERSION {
            return Err(DepthRecordError::Version(v));
        }
        let camera_id = string(&value, "camera_id")?;
        let mono_ns = uint(&value, "mono_ns")?;
        match string(&value, "kind")?.as_str() {
            "depth" => {
                let scale_source = ScaleSource::parse(&string(&value, "scale_source")?)
                    .ok_or_else(|| field_err("scale_source", "not lidar or none"))?;
                let cols = u16::try_from(uint(&value, "cols")?)
                    .map_err(|_| field_err("cols", "out of range"))?;
                let rows = u16::try_from(uint(&value, "rows")?)
                    .map_err(|_| field_err("rows", "out of range"))?;
                let near = binary(&value, "near")?;
                let measured = binary(&value, "measured")?;
                let cells = usize::from(cols) * usize::from(rows);
                if near.len() != cells {
                    return Err(field_err(
                        "near",
                        format!("{} bytes for {cells} cells", near.len()),
                    ));
                }
                if measured.len() != cells.div_ceil(8) {
                    return Err(field_err("measured", format!("{} bytes", measured.len())));
                }
                Ok(DepthRecord::Depth(DepthGrid {
                    camera_id,
                    mono_ns,
                    scale_source,
                    cols,
                    rows,
                    near,
                    measured,
                }))
            }
            "surfaces" => {
                let zones = get(&value, "zones")?
                    .as_array()
                    .ok_or_else(|| field_err("zones", "not an array"))?
                    .iter()
                    .map(decode_zone)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(DepthRecord::Surfaces(SurfacesFrame {
                    camera_id,
                    mono_ns,
                    zones,
                }))
            }
            other => Err(field_err("kind", format!("unknown kind {other:?}"))),
        }
    }
}

fn decode_zone(z: &Value) -> Result<SurfaceZone, DepthRecordError> {
    let pts = get(z, "pts")?
        .as_array()
        .ok_or_else(|| field_err("pts", "not an array"))?
        .iter()
        .map(|p| match p.as_array().map(Vec::as_slice) {
            Some([x, y]) => match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => Ok([x as f32, y as f32]),
                _ => Err(field_err("pts", "a point is not two numbers")),
            },
            _ => Err(field_err("pts", "a point is not a pair")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let num = |k: &'static str| {
        get(z, k)?
            .as_f64()
            .map(|f| f as f32)
            .ok_or_else(|| field_err(k, "not a number"))
    };
    Ok(SurfaceZone {
        pts,
        score: num("score")?,
        slope_deg: num("slope_deg")?,
    })
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(
        pairs
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect(),
    )
}

fn get<'a>(v: &'a Value, key: &'static str) -> Result<&'a Value, DepthRecordError> {
    v.as_map()
        .ok_or_else(|| field_err(key, "record is not a map"))?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
        .ok_or_else(|| field_err(key, "missing"))
}

fn uint(v: &Value, key: &'static str) -> Result<u64, DepthRecordError> {
    get(v, key)?
        .as_u64()
        .ok_or_else(|| field_err(key, "not an unsigned integer"))
}

fn string(v: &Value, key: &'static str) -> Result<String, DepthRecordError> {
    get(v, key)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| field_err(key, "not a string"))
}

fn binary(v: &Value, key: &'static str) -> Result<Vec<u8>, DepthRecordError> {
    match get(v, key)? {
        Value::Binary(b) => Ok(b.clone()),
        _ => Err(field_err(key, "not binary")),
    }
}

fn field_err(field: &'static str, reason: impl ToString) -> DepthRecordError {
    DepthRecordError::Field {
        field,
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cell_code_spans_twenty_centimetres_to_sixty_metres_on_a_log_scale() {
        assert_eq!(encode_range_m(0.2), 1);
        assert_eq!(encode_range_m(60.0), 255);
        assert_eq!(encode_range_m(0.01), 1);
        assert_eq!(encode_range_m(500.0), 255);
        assert_eq!(encode_range_m(f32::INFINITY), 255);
        assert_eq!(encode_range_m(f32::NAN), 0);
        assert_eq!(decode_range_code(0), None);
        for d in [0.5_f32, 1.8, 2.0, 5.0, 12.0, 30.0] {
            let back = decode_range_code(encode_range_m(d)).unwrap();
            // One code step is 2.3 % of the range.
            assert!((back - d).abs() / d < 0.012, "{d} -> {back}");
        }
    }

    fn grid() -> DepthGrid {
        let mut near = vec![0u8; GRID_CELLS];
        near[9 * GRID_COLS + 10] = encode_range_m(1.8);
        let mut measured = vec![0u8; GRID_MASK_BYTES];
        let i = 9 * GRID_COLS + 10;
        measured[i / 8] |= 1 << (i % 8);
        DepthGrid {
            camera_id: "main".into(),
            mono_ns: 42,
            scale_source: ScaleSource::Lidar,
            cols: GRID_COLS as u16,
            rows: GRID_ROWS as u16,
            near,
            measured,
        }
    }

    #[test]
    fn both_record_kinds_round_trip() {
        let depth = DepthRecord::Depth(grid());
        assert_eq!(
            DepthRecord::from_msgpack(&depth.to_msgpack()).unwrap(),
            depth
        );
        let surfaces = DepthRecord::Surfaces(SurfacesFrame {
            camera_id: "belly".into(),
            mono_ns: 7,
            zones: vec![SurfaceZone {
                pts: vec![[0.25, 0.75], [0.5, 0.75], [0.5, 1.0]],
                score: 0.5,
                slope_deg: 4.0,
            }],
        });
        assert_eq!(
            DepthRecord::from_msgpack(&surfaces.to_msgpack()).unwrap(),
            surfaces
        );
        assert_eq!(surfaces.camera_id(), "belly");
    }

    #[test]
    fn a_short_grid_a_wrong_version_and_an_unknown_kind_are_refused() {
        let mut short = grid();
        short.near.pop();
        assert!(matches!(
            DepthRecord::from_msgpack(&DepthRecord::Depth(short).to_msgpack()),
            Err(DepthRecordError::Field { field: "near", .. })
        ));
        let mut bytes = Vec::new();
        rmpv::encode::write_value(
            &mut bytes,
            &map(vec![
                ("kind", Value::from("depth")),
                ("v", Value::from(9)),
                ("camera_id", Value::from("main")),
                ("mono_ns", Value::from(1)),
            ]),
        )
        .unwrap();
        assert_eq!(
            DepthRecord::from_msgpack(&bytes),
            Err(DepthRecordError::Version(9))
        );
        let mut bytes = Vec::new();
        rmpv::encode::write_value(
            &mut bytes,
            &map(vec![
                ("kind", Value::from("normals")),
                ("v", Value::from(VISION_DEPTH_VERSION)),
                ("camera_id", Value::from("main")),
                ("mono_ns", Value::from(1)),
            ]),
        )
        .unwrap();
        assert!(matches!(
            DepthRecord::from_msgpack(&bytes),
            Err(DepthRecordError::Field { field: "kind", .. })
        ));
    }
}
