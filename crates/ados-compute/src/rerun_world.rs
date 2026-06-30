//! Serialize a world-model recording to a real Rerun `.rrd` the GCS World viewer
//! loads, and build that recording from a finished reconstruction's real files.
//!
//! [`rerun_log`](crate::rerun_log) builds the lightweight summary manifest (camera
//! intrinsics, poses, IMU scalars, geometry counts). This module turns a recording
//! into an actual `.rrd` via the Rerun SDK ([`RerunRecording::write_rrd`]), and
//! provides [`build_rerun_output`] which assembles a recording from the REAL files
//! a reconstruct job leaves on disk — the capture's `transforms.json` (camera
//! intrinsics + world-from-camera poses + the per-frame images) and the
//! reconstruction's point/splat `.ply` — and writes `<work_root>/<job_id>/output.rrd`.
//!
//! Entity mapping (`write_rrd`):
//! - `Pinhole`     → `rerun::Pinhole` (real intrinsics K + resolution): the frustum.
//! - `Transform3D` → `rerun::Transform3D` (real world-from-camera pose).
//! - `Scalar`      → `rerun::Scalars` (real IMU axis sample on the capture timeline).
//! - `Points3D` summary → `rerun::Boxes3D` (the real axis-aligned bounds box).
//! - `VoxelGrid`   → `rerun::Boxes3D` (the real grid extent box from origin + dims).
//! - `SplatSlab` / `Mesh3D` / `Image` summaries carry no positions/pixels, so they
//!   are not rendered FROM THE SUMMARY; their real payloads ride the side-channels.
//! - real points (the reconstructed splat / point cloud) → `rerun::Points3D`. A
//!   gaussian splat has no native Rerun archetype, so it is logged HONESTLY as
//!   `Points3D` (the gaussian centres + their DC colours) parsed from the real
//!   `.ply` — real points, never fabricated.
//! - real images (the captured keyframes) → `rerun::EncodedImage`.
//!
//! The Rust matrices are column-major; the summary matrices are row-major, so a
//! row→column transpose is applied at the boundary ([`columns_from_row_major`]).

use std::io;
use std::path::Path;

use crate::rerun_log::{RerunArchetype, RerunRecording};
use crate::{file_uri_to_path, path_to_file_uri, Output};

/// The recording timeline name; the capture's per-frame index (transforms.json has
/// no timestamps) and the keyframe milliseconds both index it as a sequence.
const TIMELINE: &str = "capture";
/// The `.rrd` artifact filename written under a finished job's work directory.
pub const RERUN_OUTPUT_FILE: &str = "output.rrd";
/// The recording's application id, embedded in the `.rrd` and shown by the viewer.
const APP_ID: &str = "ados_atlas_world_model";
/// Cap on points logged from one `.ply`; a larger cloud is decimated to a real
/// (every-k-th) subset so the recording stays bounded. Honest downsample, not fake.
const MAX_POINTS: usize = 1_000_000;
/// Spherical-harmonic DC band constant, to convert a 3DGS `f_dc_*` term to RGB.
const SH_C0: f32 = 0.282_094_79;

/// Real dense geometry attached to a recording for the `.rrd` write: the gaussian /
/// point positions (+ optional per-point colours) the summary only counts.
#[derive(Debug, Clone)]
pub(crate) struct RealPoints {
    pub entity_path: String,
    /// `None` logs the cloud as static (it shows at every time — the reconstruction
    /// is a single final artifact, not a per-frame one).
    pub timeline_ms: Option<i64>,
    pub positions: Vec<[f32; 3]>,
    pub colors: Option<Vec<[u8; 3]>>,
}

/// A real captured keyframe image attached for the `.rrd` write.
#[derive(Debug, Clone)]
pub(crate) struct RealImage {
    pub entity_path: String,
    pub timeline_ms: i64,
    pub bytes: Vec<u8>,
    pub media_type: String,
}

/// Transpose a row-major 3x3 (`[m00,m01,m02, m10,m11,m12, m20,m21,m22]`) into the
/// column array `rerun::Mat3x3::from` expects (each inner triple is a column), so
/// the Rust matrix equals the source matrix.
fn columns_from_row_major(m: &[f64; 9]) -> [[f32; 3]; 3] {
    [
        [m[0] as f32, m[3] as f32, m[6] as f32],
        [m[1] as f32, m[4] as f32, m[7] as f32],
        [m[2] as f32, m[5] as f32, m[8] as f32],
    ]
}

/// A box covering the axis-aligned bounds `[minx,miny,minz,maxx,maxy,maxz]`.
fn box_from_bounds(b: &[f64; 6]) -> rerun::Boxes3D {
    let center = [
        ((b[0] + b[3]) / 2.0) as f32,
        ((b[1] + b[4]) / 2.0) as f32,
        ((b[2] + b[5]) / 2.0) as f32,
    ];
    let half = [
        ((b[3] - b[0]).abs() / 2.0) as f32,
        ((b[4] - b[1]).abs() / 2.0) as f32,
        ((b[5] - b[2]).abs() / 2.0) as f32,
    ];
    rerun::Boxes3D::from_centers_and_half_sizes([center], [half])
}

/// A box covering a voxel grid's real extent: `dims * resolution` sized, cornered
/// at `origin`.
fn box_from_grid(dims: &[u32; 3], resolution_m: f32, origin: &[f64; 3]) -> rerun::Boxes3D {
    let half = [
        dims[0] as f32 * resolution_m / 2.0,
        dims[1] as f32 * resolution_m / 2.0,
        dims[2] as f32 * resolution_m / 2.0,
    ];
    let center = [
        origin[0] as f32 + half[0],
        origin[1] as f32 + half[1],
        origin[2] as f32 + half[2],
    ];
    rerun::Boxes3D::from_centers_and_half_sizes([center], [half])
}

impl RerunRecording {
    /// Append a camera frame: the `Pinhole` intrinsics (logged once per entity) +
    /// the world-from-camera `Transform3D` (logged every frame). `image_from_camera`
    /// and `rotation` are row-major 3x3.
    #[allow(clippy::too_many_arguments)]
    pub fn push_camera(
        &mut self,
        entity_path: &str,
        image_from_camera: [f64; 9],
        width: u32,
        height: u32,
        rotation: [f64; 9],
        translation: [f64; 3],
        timeline_ms: i64,
    ) {
        if self.logged_cameras.insert(entity_path.to_string()) {
            self.entries.push(crate::rerun_log::RerunLogEntry {
                entity_path: entity_path.to_string(),
                timeline_ms,
                archetype: RerunArchetype::Pinhole {
                    image_from_camera,
                    width,
                    height,
                },
            });
        }
        self.entries.push(crate::rerun_log::RerunLogEntry {
            entity_path: entity_path.to_string(),
            timeline_ms,
            archetype: RerunArchetype::Transform3D {
                rotation,
                translation,
            },
        });
    }

    /// Attach a real point cloud (positions + optional colours). `timeline_ms`
    /// `None` logs it static (shows at every time).
    pub fn push_points3d(
        &mut self,
        entity_path: impl Into<String>,
        positions: Vec<[f32; 3]>,
        colors: Option<Vec<[u8; 3]>>,
        timeline_ms: Option<i64>,
    ) {
        self.real_points.push(RealPoints {
            entity_path: entity_path.into(),
            timeline_ms,
            positions,
            colors,
        });
    }

    /// Attach a real encoded image (e.g. a captured JPEG keyframe).
    pub fn push_encoded_image(
        &mut self,
        entity_path: impl Into<String>,
        bytes: Vec<u8>,
        media_type: impl Into<String>,
        timeline_ms: i64,
    ) {
        self.real_images.push(RealImage {
            entity_path: entity_path.into(),
            timeline_ms,
            bytes,
            media_type: media_type.into(),
        });
    }

    /// True when the recording holds any renderable world data (a summary entry, a
    /// real point cloud, or a real image) — the gate for writing a `.rrd` at all.
    pub fn has_world_data(&self) -> bool {
        !self.entries.is_empty() || !self.real_points.is_empty() || !self.real_images.is_empty()
    }

    /// Replay the recording to a Rerun `RecordingStream` saved as `.rrd` at `path`.
    /// Maps each summary entry + each attached real payload to its SDK archetype
    /// (see the module docs). Creates the parent directory if needed.
    pub fn write_rrd(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let rec = rerun::RecordingStreamBuilder::new(APP_ID)
            .save(path)
            .map_err(io::Error::other)?;

        for entry in &self.entries {
            rec.set_time_sequence(TIMELINE, entry.timeline_ms);
            let path = entry.entity_path.as_str();
            match &entry.archetype {
                RerunArchetype::Pinhole {
                    image_from_camera,
                    width,
                    height,
                } => {
                    let arch = rerun::Pinhole::new(rerun::Mat3x3::from(columns_from_row_major(
                        image_from_camera,
                    )))
                    .with_resolution([*width as f32, *height as f32]);
                    rec.log(path, &arch).map_err(io::Error::other)?;
                }
                RerunArchetype::Transform3D {
                    rotation,
                    translation,
                } => {
                    let arch = rerun::Transform3D::from_translation_mat3x3(
                        [
                            translation[0] as f32,
                            translation[1] as f32,
                            translation[2] as f32,
                        ],
                        rerun::Mat3x3::from(columns_from_row_major(rotation)),
                    );
                    rec.log(path, &arch).map_err(io::Error::other)?;
                }
                RerunArchetype::Scalar { value } => {
                    rec.log(path, &rerun::Scalars::single(*value))
                        .map_err(io::Error::other)?;
                }
                RerunArchetype::Points3D { bounds, .. } => {
                    rec.log(path, &box_from_bounds(bounds))
                        .map_err(io::Error::other)?;
                }
                RerunArchetype::VoxelGrid {
                    dims,
                    resolution_m,
                    origin,
                } => {
                    rec.log(path, &box_from_grid(dims, *resolution_m, origin))
                        .map_err(io::Error::other)?;
                }
                // No positions/pixels in the summary; the real payloads (if any)
                // are logged below from the side-channels.
                RerunArchetype::SplatSlab { .. }
                | RerunArchetype::Mesh3D { .. }
                | RerunArchetype::Image { .. } => {}
            }
        }

        for pc in &self.real_points {
            let mut arch = rerun::Points3D::new(pc.positions.iter().copied());
            if let Some(colors) = &pc.colors {
                arch = arch.with_colors(
                    colors
                        .iter()
                        .map(|c| rerun::Color::from_rgb(c[0], c[1], c[2])),
                );
            }
            match pc.timeline_ms {
                Some(ms) => {
                    rec.set_time_sequence(TIMELINE, ms);
                    rec.log(pc.entity_path.as_str(), &arch)
                        .map_err(io::Error::other)?;
                }
                None => rec
                    .log_static(pc.entity_path.as_str(), &arch)
                    .map_err(io::Error::other)?,
            }
        }

        for img in &self.real_images {
            rec.set_time_sequence(TIMELINE, img.timeline_ms);
            let arch = rerun::EncodedImage::from_file_contents(img.bytes.clone())
                .with_media_type(img.media_type.clone());
            rec.log(img.entity_path.as_str(), &arch)
                .map_err(io::Error::other)?;
        }

        rec.flush_blocking().map_err(io::Error::other)?;
        // Drop the stream so the file sink closes the `.rrd` before we return.
        drop(rec);
        Ok(())
    }
}

/// Build the world recording from a finished reconstruction's real files: the
/// capture's `transforms.json` (cameras + images) under `dataset_input_path`, plus
/// an optional `(entity_path, .ply)` of the reconstructed geometry. Real data only
/// — a missing manifest yields no cameras, an unparseable `.ply` yields no points;
/// nothing is invented.
pub fn build_world_recording(
    dataset_input_path: Option<&Path>,
    geometry: Option<(&str, &Path)>,
) -> RerunRecording {
    let mut rec = RerunRecording::new();
    if let Some(dir) = dataset_input_path {
        add_camera_frames(&mut rec, dir);
    }
    if let Some((entity, ply)) = geometry {
        if let Ok(Some(points)) = parse_ply(ply) {
            rec.push_points3d(entity, points.positions, points.colors, None);
        }
    }
    rec
}

/// Build + write the rerun world-model `.rrd` for a finished reconstruct job and
/// return its registrable [`Output`] (kind `"rerun"`, a `file://` URI under the
/// work root the caller rewrites to the LAN artifact URL). `geometry` is the
/// reconstruction output's `(kind, uri)`; only a `.ply` `file://` URI under the
/// work root is read for points. Returns `None` when there is no real world data
/// (no cameras and no geometry), so an empty `.rrd` is never registered.
pub fn build_rerun_output(
    work_root: &Path,
    job_id: &str,
    dataset_input_path: Option<&Path>,
    geometry: Option<(&str, &str)>,
    now_ms: i64,
) -> io::Result<Option<Output>> {
    let geo_path = geometry.and_then(|(kind, uri)| {
        let path = file_uri_to_path(uri);
        let is_ply = path.extension().and_then(|e| e.to_str()) == Some("ply");
        (is_ply && path.exists()).then(|| (entity_for_kind(kind), path))
    });
    let recording = build_world_recording(
        dataset_input_path,
        geo_path.as_ref().map(|(e, p)| (*e, p.as_path())),
    );
    if !recording.has_world_data() {
        return Ok(None);
    }

    let job_dir = work_root.join(job_id);
    std::fs::create_dir_all(&job_dir)?;
    let rrd_path = job_dir.join(RERUN_OUTPUT_FILE);
    recording.write_rrd(&rrd_path)?;

    let points: usize = recording
        .real_points
        .iter()
        .map(|p| p.positions.len())
        .sum();
    let mut output = Output::new(
        format!("{job_id}-rerun"),
        job_id.to_string(),
        "rerun".into(),
        path_to_file_uri(&rrd_path.to_string_lossy()),
        now_ms,
    );
    output.meta = serde_json::json!({
        "cameras": recording.logged_cameras.len(),
        "images": recording.real_images.len(),
        "points": points,
    });
    Ok(Some(output))
}

/// The world-model entity path a reconstruction artifact kind logs under.
fn entity_for_kind(kind: &str) -> &'static str {
    match kind {
        "splat" => "world/splat",
        _ => "world/points",
    }
}

/// The `EncodedImage` media type for a captured frame, or `None` for an encoding
/// Rerun does not render as a still image (e.g. an HEVC keyframe) — in which case
/// the frame's camera is still logged, only its image is skipped (no fabrication).
fn media_type_for(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("png") => Some("image/png"),
        _ => None,
    }
}

/// Read `<dir>/transforms.json` and log each frame's camera (pinhole + pose) and,
/// when readable, its image. The poses are real (the drone's VIO/FC poses the
/// capture persisted); the per-frame index stands in for the timeline the manifest
/// does not carry. A single moving camera shares one entity; the manifest does not
/// retain a per-frame camera id, so a multi-camera rig logs under one entity.
fn add_camera_frames(rec: &mut RerunRecording, dir: &Path) {
    let Ok(bytes) = std::fs::read(dir.join("transforms.json")) else {
        return;
    };
    let Ok(scene) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return;
    };
    let Some(frames) = scene.get("frames").and_then(|f| f.as_array()) else {
        return;
    };
    const CAM: &str = "world/camera/cam";
    for (i, frame) in frames.iter().enumerate() {
        let Some(matrix) = read_mat4(frame.get("transform_matrix")) else {
            continue;
        };
        let fl_x = num(frame, &scene, "fl_x");
        let fl_y = num(frame, &scene, "fl_y");
        let cx = num(frame, &scene, "cx");
        let cy = num(frame, &scene, "cy");
        let w = num(frame, &scene, "w") as u32;
        let h = num(frame, &scene, "h") as u32;
        let k = [fl_x, 0.0, cx, 0.0, fl_y, cy, 0.0, 0.0, 1.0];
        let rotation = [
            matrix[0][0],
            matrix[0][1],
            matrix[0][2],
            matrix[1][0],
            matrix[1][1],
            matrix[1][2],
            matrix[2][0],
            matrix[2][1],
            matrix[2][2],
        ];
        let translation = [matrix[0][3], matrix[1][3], matrix[2][3]];
        rec.push_camera(CAM, k, w, h, rotation, translation, i as i64);

        if let Some(rel) = frame.get("file_path").and_then(|v| v.as_str()) {
            let img_path = dir.join(rel);
            if let Some(media) = media_type_for(&img_path) {
                if let Ok(img_bytes) = std::fs::read(&img_path) {
                    rec.push_encoded_image(format!("{CAM}/rgb"), img_bytes, media, i as i64);
                }
            }
        }
    }
}

/// A frame's intrinsic value, preferring the per-frame value over the scene-level
/// fallback (0.0 when neither is present).
fn num(frame: &serde_json::Value, scene: &serde_json::Value, key: &str) -> f64 {
    frame
        .get(key)
        .and_then(|v| v.as_f64())
        .or_else(|| scene.get(key).and_then(|v| v.as_f64()))
        .unwrap_or(0.0)
}

/// Parse a 4x4 row-major matrix from a JSON array-of-arrays, or `None` if the shape
/// is wrong.
fn read_mat4(v: Option<&serde_json::Value>) -> Option<[[f64; 4]; 4]> {
    let rows = v?.as_array()?;
    if rows.len() != 4 {
        return None;
    }
    let mut out = [[0.0f64; 4]; 4];
    for (r, row) in rows.iter().enumerate() {
        let cols = row.as_array()?;
        if cols.len() != 4 {
            return None;
        }
        for (c, cell) in cols.iter().enumerate() {
            out[r][c] = cell.as_f64()?;
        }
    }
    Some(out)
}

/// Real points parsed from a `.ply`.
pub(crate) struct PlyPoints {
    pub positions: Vec<[f32; 3]>,
    pub colors: Option<Vec<[u8; 3]>>,
}

/// One vertex property: its declared type and its byte offset within a binary row.
struct Prop {
    ty: String,
    offset: usize,
}

/// The byte width of a scalar PLY property type.
fn type_width(ty: &str) -> Option<usize> {
    match ty {
        "char" | "int8" | "uchar" | "uint8" => Some(1),
        "short" | "int16" | "ushort" | "uint16" => Some(2),
        "int" | "int32" | "uint" | "uint32" | "float" | "float32" => Some(4),
        "double" | "float64" => Some(8),
        _ => None,
    }
}

/// Convert a 3DGS spherical-harmonic DC term to an 8-bit colour channel.
fn dc_to_u8(f: f32) -> u8 {
    ((0.5 + SH_C0 * f).clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Parse a `.ply` point cloud into positions (+ colours, when present). Supports
/// ASCII and binary-little-endian (the formats Brush / nerfstudio / COLMAP export);
/// returns `Ok(None)` for an unsupported layout (e.g. binary-big-endian, or a list
/// property on the vertex element) rather than guessing. Colours come from
/// `red`/`green`/`blue` bytes when present, else the 3DGS `f_dc_0..2` SH DC term;
/// a cloud larger than [`MAX_POINTS`] is decimated to a real every-k-th subset.
pub(crate) fn parse_ply(path: &Path) -> io::Result<Option<PlyPoints>> {
    let data = std::fs::read(path)?;
    let marker = b"end_header";
    let Some(pos) = data.windows(marker.len()).position(|w| w == marker) else {
        return Ok(None);
    };
    let Ok(header) = std::str::from_utf8(&data[..pos]) else {
        return Ok(None);
    };
    let mut body = pos + marker.len();
    while body < data.len() && data[body] != b'\n' {
        body += 1;
    }
    body += 1; // skip the newline after end_header

    let mut format: Option<String> = None;
    let mut count = 0usize;
    let mut in_vertex = false;
    let mut props: Vec<Prop> = Vec::new();
    let mut name_of: Vec<String> = Vec::new();
    let mut offset = 0usize;
    for line in header.lines() {
        let mut toks = line.split_whitespace();
        match toks.next() {
            Some("format") => format = toks.next().map(str::to_string),
            Some("element") => {
                in_vertex = toks.next() == Some("vertex");
                if in_vertex {
                    count = toks.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                }
            }
            Some("property") if in_vertex => {
                let ty = toks.next().unwrap_or("");
                if ty == "list" {
                    return Ok(None); // a list property in the vertex element: unsupported
                }
                let Some(width) = type_width(ty) else {
                    return Ok(None);
                };
                let name = toks.next().unwrap_or("").to_string();
                props.push(Prop {
                    ty: ty.to_string(),
                    offset,
                });
                name_of.push(name);
                offset += width;
            }
            _ => {}
        }
    }
    if count == 0 || props.is_empty() {
        return Ok(None);
    }
    let stride = offset;
    let idx = |n: &str| name_of.iter().position(|x| x == n);
    let (Some(ix), Some(iy), Some(iz)) = (idx("x"), idx("y"), idx("z")) else {
        return Ok(None);
    };
    let rgb = match (idx("red"), idx("green"), idx("blue")) {
        (Some(r), Some(g), Some(b)) => Some((r, g, b, false)),
        _ => match (idx("f_dc_0"), idx("f_dc_1"), idx("f_dc_2")) {
            (Some(r), Some(g), Some(b)) => Some((r, g, b, true)),
            _ => None,
        },
    };

    let step = count.div_ceil(MAX_POINTS).max(1);
    let mut positions = Vec::new();
    let mut colors = rgb.map(|_| Vec::new());

    match format.as_deref() {
        Some("ascii") => {
            for (i, line) in std::str::from_utf8(&data[body..])
                .unwrap_or("")
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(count)
                .enumerate()
            {
                if i % step != 0 {
                    continue;
                }
                let f: Vec<f64> = line
                    .split_whitespace()
                    .map(|t| t.parse().unwrap_or(0.0))
                    .collect();
                if f.len() < props.len() {
                    continue;
                }
                positions.push([f[ix] as f32, f[iy] as f32, f[iz] as f32]);
                if let (Some(buf), Some((r, g, b, is_dc))) = (colors.as_mut(), rgb) {
                    buf.push(ascii_color(&f, r, g, b, is_dc));
                }
            }
        }
        Some("binary_little_endian") => {
            for i in 0..count {
                if i % step != 0 {
                    continue;
                }
                let start = body + i * stride;
                let Some(row) = data.get(start..start + stride) else {
                    break;
                };
                positions.push([
                    read_f32_le(row, &props[ix]),
                    read_f32_le(row, &props[iy]),
                    read_f32_le(row, &props[iz]),
                ]);
                if let (Some(buf), Some((r, g, b, is_dc))) = (colors.as_mut(), rgb) {
                    buf.push(binary_color(row, &props, r, g, b, is_dc));
                }
            }
        }
        _ => return Ok(None),
    }

    if positions.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlyPoints { positions, colors }))
}

/// A colour triple from an ASCII vertex's parsed fields.
fn ascii_color(f: &[f64], r: usize, g: usize, b: usize, is_dc: bool) -> [u8; 3] {
    if is_dc {
        [
            dc_to_u8(f[r] as f32),
            dc_to_u8(f[g] as f32),
            dc_to_u8(f[b] as f32),
        ]
    } else {
        [f[r] as u8, f[g] as u8, f[b] as u8]
    }
}

/// A colour triple from a binary vertex row.
fn binary_color(row: &[u8], props: &[Prop], r: usize, g: usize, b: usize, is_dc: bool) -> [u8; 3] {
    if is_dc {
        [
            dc_to_u8(read_f32_le(row, &props[r])),
            dc_to_u8(read_f32_le(row, &props[g])),
            dc_to_u8(read_f32_le(row, &props[b])),
        ]
    } else {
        [
            row[props[r].offset],
            row[props[g].offset],
            row[props[b].offset],
        ]
    }
}

/// Read a property value from a binary-little-endian row as `f32`, honouring its
/// declared type (a `double` is read as 8 bytes, a `float` as 4).
fn read_f32_le(row: &[u8], prop: &Prop) -> f32 {
    let o = prop.offset;
    match prop.ty.as_str() {
        "double" | "float64" => row
            .get(o..o + 8)
            .and_then(|b| b.try_into().ok())
            .map(|b| f64::from_le_bytes(b) as f32)
            .unwrap_or(0.0),
        _ => row
            .get(o..o + 4)
            .and_then(|b| b.try_into().ok())
            .map(f32::from_le_bytes)
            .unwrap_or(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite_output_to_artifact_url;

    fn write(dir: &Path, rel: &str, bytes: &[u8]) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// A two-frame `transforms.json` + its two JPEG keyframes, the shape the
    /// keyframe persister writes (scene-level intrinsics + per-frame poses).
    fn write_capture(dir: &Path) {
        let scene = serde_json::json!({
            "camera_model": "OPENCV", "fl_x": 900.0, "fl_y": 900.0,
            "cx": 640.0, "cy": 360.0, "w": 1280, "h": 720,
            "frames": [
                { "file_path": "images/0.jpg",
                  "transform_matrix": [[1.0,0.0,0.0,1.0],[0.0,1.0,0.0,2.0],[0.0,0.0,1.0,3.0],[0.0,0.0,0.0,1.0]] },
                { "file_path": "images/1.jpg",
                  "transform_matrix": [[1.0,0.0,0.0,1.1],[0.0,1.0,0.0,2.1],[0.0,0.0,1.0,3.1],[0.0,0.0,0.0,1.0]] }
            ]
        });
        write(
            dir,
            "transforms.json",
            serde_json::to_vec(&scene).unwrap().as_slice(),
        );
        write(dir, "images/0.jpg", &[0xFF, 0xD8, 0xFF, 0x00]);
        write(dir, "images/1.jpg", &[0xFF, 0xD8, 0xFF, 0x01]);
    }

    #[test]
    fn write_rrd_emits_a_non_empty_recording_with_the_logged_entities() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = RerunRecording::new();
        rec.push_camera(
            "world/camera/cam",
            [900.0, 0.0, 640.0, 0.0, 900.0, 360.0, 0.0, 0.0, 1.0],
            1280,
            720,
            [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            [1.0, 2.0, 3.0],
            0,
        );
        rec.push_points3d(
            "world/splat",
            vec![[0.0, 0.0, 0.0], [1.0, 1.0, 1.0]],
            Some(vec![[255, 0, 0], [0, 255, 0]]),
            None,
        );
        rec.push_encoded_image(
            "world/camera/cam/rgb",
            vec![0xFF, 0xD8, 0xFF, 0x00],
            "image/jpeg",
            0,
        );

        let path = dir.path().join("scene.rrd");
        rec.write_rrd(&path).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.is_empty(), "the .rrd must not be empty");
        // The entity paths are carried as UTF-8 strings in the recording's chunks.
        let blob = String::from_utf8_lossy(&bytes);
        assert!(blob.contains("world/camera/cam"), "camera entity present");
        assert!(blob.contains("world/splat"), "splat points entity present");
    }

    #[test]
    fn build_world_recording_reads_cameras_and_a_real_ply() {
        let dir = tempfile::tempdir().unwrap();
        write_capture(dir.path());
        let ply = dir.path().join("cloud.ply");
        std::fs::write(
            &ply,
            "ply\nformat ascii 1.0\nelement vertex 2\nproperty float x\nproperty float y\nproperty float z\nproperty uchar red\nproperty uchar green\nproperty uchar blue\nend_header\n0 0 0 255 0 0\n1 2 3 0 255 0\n",
        )
        .unwrap();

        let rec = build_world_recording(Some(dir.path()), Some(("world/points", &ply)));
        // Two camera frames: one pinhole (logged once) + two transforms.
        assert_eq!(rec.logged_cameras.len(), 1);
        assert_eq!(rec.real_images.len(), 2);
        assert_eq!(rec.real_points.len(), 1);
        assert_eq!(rec.real_points[0].positions.len(), 2);
        assert_eq!(rec.real_points[0].entity_path, "world/points");
        assert!(rec.has_world_data());
    }

    #[test]
    fn build_rerun_output_registers_a_rerun_artifact_rewritten_to_http() {
        let work = tempfile::tempdir().unwrap();
        let ds = work.path().join("ds-1");
        write_capture(&ds);

        let out = build_rerun_output(work.path(), "job-1", Some(&ds), None, 100)
            .unwrap()
            .expect("a recording with real cameras registers a rerun output");
        assert_eq!(out.kind, "rerun");
        assert_eq!(out.meta["cameras"], 1);
        assert_eq!(out.meta["images"], 2);
        // The artifact is a real, non-empty .rrd under <work_root>/<job_id>/.
        let rrd = work.path().join("job-1").join(RERUN_OUTPUT_FILE);
        assert!(rrd.exists());
        assert!(std::fs::metadata(&rrd).unwrap().len() > 0);
        assert!(out.uri.starts_with("file://"));

        // The file:// URI rewrites to the fetchable LAN artifact URL the GCS loads.
        let mut rewritten = out;
        rewrite_output_to_artifact_url(&mut rewritten, work.path(), "http://node.local:8092");
        assert_eq!(
            rewritten.uri,
            "http://node.local:8092/artifacts/job-1/output.rrd"
        );
    }

    #[test]
    fn build_rerun_output_is_none_with_no_world_data() {
        let work = tempfile::tempdir().unwrap();
        // No dataset and a mock:// (non-.ply) geometry uri: nothing real to record.
        let out = build_rerun_output(
            work.path(),
            "job-x",
            None,
            Some(("splat", "mock://splat/ds")),
            7,
        )
        .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn parse_ply_reads_ascii_positions_and_colors() {
        let dir = tempfile::tempdir().unwrap();
        let ply = dir.path().join("a.ply");
        std::fs::write(
            &ply,
            "ply\nformat ascii 1.0\nelement vertex 2\nproperty float x\nproperty float y\nproperty float z\nproperty uchar red\nproperty uchar green\nproperty uchar blue\nend_header\n0.5 1.0 -2.0 255 128 0\n3.0 4.0 5.0 10 20 30\n",
        )
        .unwrap();
        let pts = parse_ply(&ply).unwrap().unwrap();
        assert_eq!(pts.positions, vec![[0.5, 1.0, -2.0], [3.0, 4.0, 5.0]]);
        assert_eq!(pts.colors.unwrap(), vec![[255, 128, 0], [10, 20, 30]]);
    }

    #[test]
    fn parse_ply_reads_binary_little_endian_positions() {
        let dir = tempfile::tempdir().unwrap();
        let ply = dir.path().join("b.ply");
        let mut bytes = b"ply\nformat binary_little_endian 1.0\nelement vertex 2\nproperty float x\nproperty float y\nproperty float z\nend_header\n".to_vec();
        for v in [[1.0f32, 2.0, 3.0], [-4.0, 5.0, 6.0]] {
            for c in v {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
        }
        std::fs::write(&ply, &bytes).unwrap();
        let pts = parse_ply(&ply).unwrap().unwrap();
        assert_eq!(pts.positions, vec![[1.0, 2.0, 3.0], [-4.0, 5.0, 6.0]]);
        assert!(pts.colors.is_none());
    }

    #[test]
    fn parse_ply_converts_3dgs_f_dc_to_color() {
        let dir = tempfile::tempdir().unwrap();
        let ply = dir.path().join("gs.ply");
        // f_dc = 0 -> 0.5 grey (128); the SH DC band maps the splat colour.
        std::fs::write(
            &ply,
            "ply\nformat ascii 1.0\nelement vertex 1\nproperty float x\nproperty float y\nproperty float z\nproperty float f_dc_0\nproperty float f_dc_1\nproperty float f_dc_2\nend_header\n0 0 0 0 0 0\n",
        )
        .unwrap();
        let pts = parse_ply(&ply).unwrap().unwrap();
        assert_eq!(pts.colors.unwrap(), vec![[128, 128, 128]]);
    }
}
