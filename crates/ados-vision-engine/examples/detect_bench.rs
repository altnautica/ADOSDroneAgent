//! Time the ONNX detector on recorded frames.
//!
//! Runs the engine's in-process ONNX backend (the same load, preprocess,
//! inference and decode path the vision engine runs) over the first `N` frames
//! of a directory and prints each frame's latency, then the mean, median and
//! 95th percentile. Used to compare a companion board with a constrained
//! simulation host running the same workload.
//!
//! Frames are binary PPM (`P6`, 8-bit RGB), which ffmpeg writes directly:
//!
//! ```text
//! ffmpeg -i flight.mp4 -frames:v 100 frames/frame_%04d.ppm
//! ```
//!
//! Usage (from `crates/`):
//!
//! ```text
//! cargo run --release -p ados-vision-engine --features onnx --example detect_bench -- \
//!     --model yolox_tiny_person_640.onnx --frames frames/ [--n 100] [--warmup 3]
//! ```
//!
//! A self-describing model file (`ados.*` metadata) sets its own head, classes
//! and input size; `--input WxH`, `--head yolo5|yolo8` and `--classes a,b`
//! cover a file without them.

use std::path::{Path, PathBuf};
use std::time::Instant;

use ados_protocol::framebus::{
    DetectionHead, FrameFormat, ModelExecution, ModelKind, ModelMetadata,
};
use ados_vision_engine::backend::{OnnxBackend, VisionBackend};
use anyhow::{anyhow, bail, Context, Result};

struct Args {
    model: PathBuf,
    frames: PathBuf,
    n: usize,
    warmup: usize,
    input: (u32, u32),
    head: DetectionHead,
    classes: Vec<String>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        model: PathBuf::new(),
        frames: PathBuf::new(),
        n: 100,
        warmup: 3,
        input: (640, 640),
        head: DetectionHead::Yolo8,
        classes: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--model" => args.model = PathBuf::from(value()?),
            "--frames" => args.frames = PathBuf::from(value()?),
            "--n" => args.n = value()?.parse().context("--n")?,
            "--warmup" => args.warmup = value()?.parse().context("--warmup")?,
            "--input" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or_else(|| anyhow!("--input WxH"))?;
                args.input = (w.parse()?, h.parse()?);
            }
            "--head" => {
                args.head = match value()?.as_str() {
                    "yolo5" => DetectionHead::Yolo5,
                    "yolo8" => DetectionHead::Yolo8,
                    other => bail!("unknown --head {other}"),
                }
            }
            "--classes" => {
                args.classes = value()?
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            other => bail!("unknown argument {other}"),
        }
    }
    if args.model.as_os_str().is_empty() || args.frames.as_os_str().is_empty() {
        bail!("usage: detect_bench --model <file.onnx> --frames <dir> [--n N] [--warmup N]");
    }
    Ok(args)
}

/// Read a binary PPM (`P6`, maxval 255) into `(width, height, rgb24)`.
fn read_ppm(path: &Path) -> Result<(u32, u32, Vec<u8>)> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    // Header: magic, width, height, maxval, separated by whitespace, with
    // `#` comments allowed; a single whitespace byte precedes the pixels.
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

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&args.frames)
        .with_context(|| format!("read {}", args.frames.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ppm")))
        .collect();
    paths.sort();
    paths.truncate(args.n);
    if paths.is_empty() {
        bail!("no .ppm frames in {}", args.frames.display());
    }

    let meta = ModelMetadata {
        id: "detect-bench".into(),
        kind: ModelKind::Detection,
        execution: ModelExecution::EngineRun,
        input_width: args.input.0,
        input_height: args.input.1,
        input_format: FrameFormat::Rgb24,
        output_classes: args.classes.clone(),
        model_path: Some(args.model.to_string_lossy().into_owned()),
        head: args.head,
    };
    let model = OnnxBackend::new()
        .load(&meta)
        .with_context(|| format!("load {}", args.model.display()))?;

    let frames: Vec<(u32, u32, Vec<u8>)> =
        paths.iter().map(|p| read_ppm(p)).collect::<Result<_>>()?;
    for (w, h, px) in frames.iter().cycle().take(args.warmup) {
        model.infer(px, *w, *h, FrameFormat::Rgb24)?;
    }

    let mut latencies = Vec::with_capacity(frames.len());
    for (path, (w, h, px)) in paths.iter().zip(&frames) {
        let t0 = Instant::now();
        let dets = model.infer(px, *w, *h, FrameFormat::Rgb24)?;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        latencies.push(ms);
        println!(
            "{}\t{w}x{h}\t{ms:.2} ms\t{} detections",
            path.file_name().unwrap_or_default().to_string_lossy(),
            dets.len()
        );
    }
    let mean = latencies.iter().sum::<f64>() / latencies.len() as f64;
    let mut sorted = latencies.clone();
    sorted.sort_by(f64::total_cmp);
    println!(
        "frames {}\tmean {mean:.2} ms\tp50 {:.2} ms\tp95 {:.2} ms\tmax {:.2} ms",
        sorted.len(),
        percentile(&sorted, 0.5),
        percentile(&sorted, 0.95),
        sorted[sorted.len() - 1]
    );
    Ok(())
}
