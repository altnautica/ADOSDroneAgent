//! Replay a recorded follow sequence through the vision engine and report
//! the detector's batch time, its whole-frame and crop passes, and how often
//! the subject is found, by range.
//!
//! The engine is the one the vision service runs: the in-process ONNX backend,
//! the single-object tracker and the crop-primary batch plan. Frames are binary
//! PPM (`P6`, 8-bit RGB) in a directory, with a `truth.tsv` beside them: one
//! line per frame, `file x y width height range_m`, the subject's box in
//! frame pixels and its distance from the camera. Frames are stamped 1 /
//! `--rate-hz` apart and run back to back.
//!
//! Two modes:
//!
//! - `follow` (default): the subject is designated on the first frame from its
//!   true box, then every frame is one detector batch. A frame counts as found
//!   when the batch carries the designated track `locked` on a box with IoU at
//!   least 0.3 against the truth.
//! - `whole`: every frame is one whole-frame pass of `--model` with no
//!   tracker. A frame counts as found when any person box has IoU at least 0.3
//!   against the truth.
//!
//! Usage (from `crates/`):
//!
//! ```text
//! cargo run --release -p ados-vision-engine --features onnx --example follow_bench -- \
//!     --model yolox_tiny_person_640.onnx [--crop-model yolox_tiny_person_320.onnx] \
//!     --frames walk/ [--mode follow|whole] [--threads 2] [--rate-hz 10] [--full-frame-hz 2] \
//!     [--reid osnet_x0_25_reid_256x128.onnx] [--bins 4,8,12,15,20,25] [--verbose]
//! ```
//!
//! `--full-frame-hz 0` runs the whole frame on every batch, with the crop
//! beside it only when it magnifies the subject: the schedule before the crop
//! became the primary pass.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use ados_protocol::framebus::{
    BoundingBox, Detection, DetectionHead, FrameDescriptor, FrameFormat, LockState, ModelExecution,
    ModelKind, ModelMetadata, FRAMEBUS_DESCRIPTOR_VERSION, TIMEBASE_HOST,
};
use ados_vision::tracker::TrackerConfig;
use ados_vision_engine::backend::OnnxBackend;
use ados_vision_engine::engine::{DetectionPolicy, VisionEngine};
use anyhow::{anyhow, bail, Context, Result};

const CAMERA: &str = "front";
const DETECTOR: &str = "detector";
const CROP: &str = "detector.crop";
const REID: &str = "reid";
const MIN_IOU: f32 = 0.3;

struct Args {
    model: PathBuf,
    crop_model: Option<PathBuf>,
    reid: Option<PathBuf>,
    frames: PathBuf,
    whole_only: bool,
    threads: usize,
    rate_hz: f64,
    full_frame_hz: f64,
    bins: Vec<f32>,
    verbose: bool,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        model: PathBuf::new(),
        crop_model: None,
        reid: None,
        frames: PathBuf::new(),
        whole_only: false,
        threads: 2,
        rate_hz: 10.0,
        full_frame_hz: 2.0,
        bins: vec![4.0, 8.0, 12.0, 15.0, 20.0, 25.0],
        verbose: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--model" => a.model = PathBuf::from(value()?),
            "--crop-model" => a.crop_model = Some(PathBuf::from(value()?)),
            "--reid" => a.reid = Some(PathBuf::from(value()?)),
            "--frames" => a.frames = PathBuf::from(value()?),
            "--mode" => {
                a.whole_only = match value()?.as_str() {
                    "follow" => false,
                    "whole" => true,
                    other => bail!("unknown --mode {other}"),
                }
            }
            "--threads" => a.threads = value()?.parse().context("--threads")?,
            "--rate-hz" => a.rate_hz = value()?.parse().context("--rate-hz")?,
            "--full-frame-hz" => a.full_frame_hz = value()?.parse().context("--full-frame-hz")?,
            "--bins" => {
                a.bins = value()?
                    .split(',')
                    .map(|s| s.trim().parse::<f32>())
                    .collect::<Result<_, _>>()
                    .context("--bins")?
            }
            "--verbose" => a.verbose = true,
            other => bail!("unknown argument {other}"),
        }
    }
    if a.model.as_os_str().is_empty() || a.frames.as_os_str().is_empty() {
        bail!("usage: follow_bench --model <file.onnx> --frames <dir> [options]");
    }
    if a.rate_hz.is_nan() || a.rate_hz <= 0.0 {
        bail!("--rate-hz must be positive");
    }
    Ok(a)
}

/// Read a binary PPM (`P6`, maxval 255) into `(width, height, rgb24)`.
fn read_ppm(path: &Path) -> Result<(u32, u32, Vec<u8>)> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut fields = Vec::with_capacity(4);
    let mut i = 0;
    while fields.len() < 4 {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if start == i {
            bail!("{}: truncated PPM header", path.display());
        }
        fields.push(std::str::from_utf8(&bytes[start..i])?.to_string());
    }
    if fields[0] != "P6" || fields[3] != "255" {
        bail!(
            "{}: only 8-bit binary PPM (P6) is supported",
            path.display()
        );
    }
    let (w, h): (u32, u32) = (fields[1].parse()?, fields[2].parse()?);
    let pixels = bytes
        .get(i + 1..i + 1 + (w * h * 3) as usize)
        .ok_or_else(|| anyhow!("{}: truncated pixel data", path.display()))?;
    Ok((w, h, pixels.to_vec()))
}

/// One frame of the sequence: its file, the subject's true box and range.
struct Truth {
    file: String,
    bbox: BoundingBox,
    range_m: f32,
}

fn read_truth(dir: &Path) -> Result<Vec<Truth>> {
    let path = dir.join("truth.tsv");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() != 6 {
                bail!("{}: expected `file x y w h range_m`: {l}", path.display());
            }
            let n = |i: usize| f[i].parse::<f32>().with_context(|| l.to_string());
            Ok(Truth {
                file: f[0].to_string(),
                bbox: BoundingBox {
                    x: n(1)?,
                    y: n(2)?,
                    width: n(3)?,
                    height: n(4)?,
                },
                range_m: n(5)?,
            })
        })
        .collect()
}

fn detector_meta(id: &str, path: &Path) -> ModelMetadata {
    // The exported files state their own input size, head and classes.
    ModelMetadata {
        id: id.into(),
        kind: ModelKind::Detection,
        execution: ModelExecution::EngineRun,
        input_width: 640,
        input_height: 640,
        input_format: FrameFormat::Rgb24,
        output_classes: vec!["person".into()],
        model_path: Some(path.to_string_lossy().into_owned()),
        head: DetectionHead::Yolo5,
    }
}

fn iou(a: &BoundingBox, b: &BoundingBox) -> f32 {
    let iw = ((a.x + a.width).min(b.x + b.width) - a.x.max(b.x)).max(0.0);
    let ih = ((a.y + a.height).min(b.y + b.height) - a.y.max(b.y)).max(0.0);
    let inter = iw * ih;
    let union = a.width * a.height + b.width * b.height - inter;
    if union > 0.0 {
        inter / union
    } else {
        0.0
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn descriptor(frame_id: u64, ts_ns: u64, w: u32, h: u32) -> FrameDescriptor {
    FrameDescriptor {
        v: FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: CAMERA.into(),
        frame_id,
        ts_ns,
        ts_sensor_ns: None,
        exposure_ns: None,
        trigger_seq: None,
        timebase: TIMEBASE_HOST,
        width: w,
        height: h,
        format: FrameFormat::Rgb24,
        seq: frame_id,
        slot: 0,
        shm_name: String::new(),
        byte_len: w * h * 3,
        hfov_deg: None,
        mount_pitch_deg: None,
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let truth = read_truth(&args.frames)?;
    if truth.is_empty() {
        bail!("no frames in {}/truth.tsv", args.frames.display());
    }
    let frames: Vec<(u32, u32, Vec<u8>)> = truth
        .iter()
        .map(|t| read_ppm(&args.frames.join(&t.file)))
        .collect::<Result<_>>()?;

    let engine: Arc<VisionEngine> = VisionEngine::with_tracker_reid(
        Box::new(OnnxBackend::new(args.threads, 1)),
        4,
        true,
        TrackerConfig::default(),
        args.reid.is_some(),
        args.reid.as_ref().map(|_| REID.to_string()),
        DetectionPolicy::default(),
    );
    let (_, loaded) = engine
        .register_model(detector_meta(DETECTOR, &args.model))
        .await?;
    if !loaded {
        bail!("{} did not load", args.model.display());
    }
    let crop_id = match &args.crop_model {
        Some(p) => {
            let (_, loaded) = engine.register_model(detector_meta(CROP, p)).await?;
            if !loaded {
                bail!("{} did not load", p.display());
            }
            Some(CROP.to_string())
        }
        None => None,
    };
    if let Some(p) = &args.reid {
        let meta = ModelMetadata {
            id: REID.into(),
            kind: ModelKind::Tracking,
            execution: ModelExecution::EngineRun,
            input_width: 128,
            input_height: 256,
            input_format: FrameFormat::Rgb24,
            output_classes: Vec::new(),
            model_path: Some(p.to_string_lossy().into_owned()),
            head: DetectionHead::Yolo8,
        };
        engine.register_model(meta).await?;
    }
    let full_ns = if args.full_frame_hz > 0.0 && args.full_frame_hz < args.rate_hz {
        (1e9 / args.full_frame_hz).round() as u64
    } else {
        0
    };
    engine.configure_crop_primary(DETECTOR, crop_id, full_ns);

    let step_ns = (1e9 / args.rate_hz).round() as u64;
    let t0_ns = 1_000_000_000u64;
    // Warm the sessions on the first frame.
    let (w0, h0, px0) = &frames[0];
    for _ in 0..3 {
        engine
            .infer(DETECTOR, px0, *w0, *h0, FrameFormat::Rgb24)
            .await?;
    }

    let mut found = vec![(0usize, 0usize); args.bins.len()];
    let bin_of = |r: f32| args.bins.iter().rposition(|b| r >= *b);
    let mut batch_ms = Vec::with_capacity(frames.len());
    let mut designated: Option<u64> = None;
    let wall = Instant::now();
    for (i, (t, (w, h, px))) in truth.iter().zip(&frames).enumerate() {
        let desc = descriptor(i as u64, t0_ns + i as u64 * step_ns, *w, *h);
        let start = Instant::now();
        let hit = if args.whole_only {
            let dets = engine
                .infer(DETECTOR, px, *w, *h, FrameFormat::Rgb24)
                .await?;
            dets.iter()
                .any(|d| d.bbox.is_some_and(|b| iou(&b, &t.bbox) >= MIN_IOU))
        } else {
            if designated.is_none() {
                let target = Detection {
                    bbox: Some(t.bbox),
                    class_label: "person".into(),
                    confidence: 0.9,
                    track_id: None,
                    assoc_confidence: None,
                    lock_state: None,
                    attributes: None,
                    mask: None,
                    keypoints: None,
                    depth: None,
                    world_pos: None,
                };
                designated = engine.designate(CAMERA, &target).await;
            }
            let batch = engine.infer_and_publish(DETECTOR, &desc, px).await?;
            batch.detections.iter().any(|d| {
                d.track_id.is_some()
                    && d.track_id == designated
                    && d.lock_state == Some(LockState::Locked)
                    && d.bbox.is_some_and(|b| iou(&b, &t.bbox) >= MIN_IOU)
            })
        };
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        batch_ms.push(ms);
        if let Some(b) = bin_of(t.range_m) {
            found[b].0 += 1;
            found[b].1 += usize::from(hit);
        }
        if args.verbose {
            println!(
                "{}\t{:.1} m\th {:.0}\t{ms:.1} ms\t{}",
                t.file,
                t.range_m,
                t.bbox.height,
                if hit { "found" } else { "missed" }
            );
        }
    }
    let elapsed = wall.elapsed().as_secs_f64();

    let mut sorted = batch_ms.clone();
    sorted.sort_by(f64::total_cmp);
    let p50 = percentile(&sorted, 0.5);
    println!(
        "mode {}\tthreads {}\tcrop model {}\tfull-frame {} Hz\tframes {}",
        if args.whole_only { "whole" } else { "follow" },
        args.threads,
        args.crop_model
            .as_deref()
            .map_or("none".into(), |p| p.display().to_string()),
        args.full_frame_hz,
        sorted.len()
    );
    println!(
        "batch ms\tmean {:.1}\tp50 {p50:.1}\tp95 {:.1}\tmax {:.1}\tback-to-back {:.1} Hz\tat a {} Hz pace {:.1} Hz",
        sorted.iter().sum::<f64>() / sorted.len() as f64,
        percentile(&sorted, 0.95),
        sorted[sorted.len() - 1],
        sorted.len() as f64 / elapsed,
        args.rate_hz,
        args.rate_hz.min(1000.0 / p50.max(1e-3)),
    );
    if !args.whole_only {
        let status = engine.detection_status(&[CAMERA.to_string()], 0);
        if let Some(c) = status.cameras.get(CAMERA) {
            let b = &c.batches;
            println!(
                "batches {}\twhole-frame {} (on the subject {}, found {})\tcrop {} (found {})\tEWMA ms: whole {:.1} crop {:.1} preprocess {:.1} inference {:.1} decode {:.2} track {:.1}",
                b.batches,
                b.full_frame_runs,
                c.subject_full_runs,
                c.subject_full_hits,
                c.crop_runs,
                c.crop_hits,
                b.full_frame_us as f64 / 1000.0,
                b.crop_us as f64 / 1000.0,
                b.preprocess_us as f64 / 1000.0,
                b.inference_us as f64 / 1000.0,
                b.decode_us as f64 / 1000.0,
                b.track_us as f64 / 1000.0,
            );
        }
    }
    println!("range_m\tframes\tfound\trecall");
    for (i, (n, k)) in found.iter().enumerate() {
        let hi = args
            .bins
            .get(i + 1)
            .map_or("+".to_string(), |b| format!("{b}"));
        let recall = if *n > 0 { *k as f64 / *n as f64 } else { 0.0 };
        println!("{}-{hi}\t{n}\t{k}\t{recall:.2}", args.bins[i]);
    }
    Ok(())
}
