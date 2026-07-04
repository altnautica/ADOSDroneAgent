//! Real reconstructor backends, behind the [`Reconstructor`] trait.
//!
//! Each backend is a thin integration over a third-party tool the compute node
//! shells out to: Brush (a Rust gaussian-splat trainer, Metal/Vulkan/wgpu, no
//! CUDA), nerfstudio/splatfacto (Python, CUDA or MPS), and COLMAP (SfM poses +
//! sparse cloud, the seed pre-pass). The tool itself is not
//! Altnautica code; this module owns the COMMAND it runs (program + args derived
//! from the dataset + params), the output-URI convention, and the result parse.
//!
//! The command builder and the gaussian-count parse are pure and unit-tested.
//! The execution path runs only when the tool is on `PATH` (a bench / GPU node);
//! [`select_reconstructor`] falls back to the [`MockReconstructor`] when no real
//! backend is installed, so CI and a node with no GPU exercise the queue,
//! scheduler, and output paths end to end with no third-party dependency.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::{ComputeError, Dataset, MockReconstructor, ReconstructOutput, Reconstructor};

/// A reconstructor that picks the right backend per job (reading the `backend`
/// param hint via [`select_reconstructor`]) and writes artifacts under one work
/// root. The daemon holds one of these so a job hinting `brush` runs Brush when it
/// is installed, falling back to the mock backend (CI / no-GPU) — without the
/// scheduler having to choose a backend per job.
pub struct SelectingReconstructor {
    work_root: PathBuf,
}

impl SelectingReconstructor {
    /// A selector that writes artifacts under `work_root`.
    pub fn new(work_root: impl Into<PathBuf>) -> Self {
        Self {
            work_root: work_root.into(),
        }
    }
}

impl Reconstructor for SelectingReconstructor {
    fn name(&self) -> &str {
        "selecting"
    }

    fn reconstruct(
        &self,
        dataset: &Dataset,
        params: &serde_json::Value,
    ) -> Result<ReconstructOutput, ComputeError> {
        select_reconstructor(params, &self.work_root).reconstruct(dataset, params)
    }
}

/// The reconstruction tools the compute node can drive. Each maps to a program
/// name, the artifact kind it produces, and the output file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconstructorKind {
    /// Brush: Rust gaussian-splat trainer (Metal / Vulkan / wgpu, no CUDA).
    /// Portable — the universal fallback; random-inits so it trains from a
    /// pose-only dataset. Slower than a native-GPU trainer (wgpu overhead).
    Brush,
    /// msplat: native-Metal gaussian-splat trainer for Apple Silicon (~10x faster
    /// than Brush on an M-series GPU). Inits from a `points3D.ply` sitting next to
    /// the Nerfstudio `transforms.json` (its own reader; no COLMAP binary), so the
    /// seed step writes that cloud first.
    Msplat,
    /// nerfstudio / splatfacto: high-quality gaussian splat (Python, CUDA/MPS).
    Nerfstudio,
    /// COLMAP: structure-from-motion poses + a sparse point cloud (the pre-pass).
    Colmap,
}

impl ReconstructorKind {
    /// Parse a backend hint (the `backend` job param) to a tool, if known.
    pub fn from_hint(hint: &str) -> Option<Self> {
        match hint.to_ascii_lowercase().as_str() {
            "brush" => Some(Self::Brush),
            "msplat" => Some(Self::Msplat),
            "nerfstudio" | "splatfacto" => Some(Self::Nerfstudio),
            "colmap" => Some(Self::Colmap),
            _ => None,
        }
    }

    /// The program invoked on `PATH`.
    pub fn program(self) -> &'static str {
        match self {
            Self::Brush => "brush",
            Self::Msplat => "msplat",
            Self::Nerfstudio => "ns-train",
            Self::Colmap => "colmap",
        }
    }

    /// A stable backend name for logs + the job result.
    pub fn name(self) -> &'static str {
        match self {
            Self::Brush => "brush",
            Self::Msplat => "msplat",
            Self::Nerfstudio => "nerfstudio",
            Self::Colmap => "colmap",
        }
    }

    /// The artifact kind this tool produces (matches the world-model topics).
    pub fn artifact_kind(self) -> &'static str {
        match self {
            Self::Brush | Self::Msplat | Self::Nerfstudio => "splat",
            Self::Colmap => "pointcloud",
        }
    }

    /// The output file extension the artifact lands as.
    pub fn output_ext(self) -> &'static str {
        match self {
            Self::Brush | Self::Msplat | Self::Nerfstudio => "ply",
            Self::Colmap => "ply",
        }
    }
}

/// A fully-built reconstruction command: the program, its args, the working
/// directory the tool runs in, and the URI the artifact lands at. Built purely
/// from a dataset + params so it can be asserted in a test without executing.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconstructCommand {
    pub program: String,
    pub args: Vec<String>,
    pub workdir: PathBuf,
    pub output_path: PathBuf,
    pub output_uri: String,
}

/// A reconstructor that drives a real third-party tool via the command line.
pub struct CliReconstructor {
    kind: ReconstructorKind,
    work_root: PathBuf,
}

impl CliReconstructor {
    /// A reconstructor for `kind`, writing artifacts under `work_root`.
    pub fn new(kind: ReconstructorKind, work_root: impl Into<PathBuf>) -> Self {
        Self {
            kind,
            work_root: work_root.into(),
        }
    }

    /// The tool this reconstructor drives.
    pub fn kind(&self) -> ReconstructorKind {
        self.kind
    }

    /// Build the command for `dataset`. Pure: the input is the prior pipeline
    /// stage's output (`params.input_uri`, a `file://` URI) when chained, else
    /// the dataset's `input_path`, else `<work_root>/<id>/input`. COLMAP writes a
    /// workspace directory; the trainers write `output.<ext>`. The quality preset
    /// comes from `params.steps` (splat trainers) when present.
    pub fn command(&self, dataset: &Dataset, params: &serde_json::Value) -> ReconstructCommand {
        let workdir = self.work_root.join(&dataset.id);
        // COLMAP's "output" is its workspace directory (sparse/dense models),
        // not a single file; the trainers export one artifact file.
        let output_path = match self.kind {
            ReconstructorKind::Colmap => workdir.join("colmap"),
            _ => workdir.join(format!("output.{}", self.kind.output_ext())),
        };
        // A pipeline stage consumes the prior stage's output (input_uri) so the
        // chain is wired, not just recorded as lineage; stage 0 falls back to the
        // dataset's input_path, then to a conventional default under the workdir.
        let input = params
            .get("input_uri")
            .and_then(|v| v.as_str())
            .map(file_uri_to_path)
            .or_else(|| {
                dataset
                    .meta
                    .get("input_path")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| workdir.join("input"));

        let input_s = input.to_string_lossy().into_owned();
        let output_s = output_path.to_string_lossy().into_owned();
        let output_uri = path_to_file_uri(&output_s);

        let args: Vec<String> = match self.kind {
            ReconstructorKind::Brush => {
                // Real Brush CLI (validated against `brush --help`): the dataset is
                // a positional argument; training length is `--total-train-iters`;
                // the artifact lands at `<--export-path>/<--export-name>`. Point the
                // export at the absolute workdir with a literal filename so the .ply
                // is written exactly at `output_path`, and set `--export-every` to
                // the step count so a single final export is produced at the end.
                let steps = params
                    .get("steps")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30000);
                // Bound densification. Without these, per-step cost blows up: Brush
                // defaults `--growth-stop-iter` to 15000 (so any run <=30k densifies
                // the WHOLE time) and `--max-splats` to 10M (no real cap), which grew
                // a small scene to 3.45M gaussians and made each step progressively
                // slower (hours). Stop growth before the last step (leaving a
                // refine-only tail) and cap the count. Both are overridable per job;
                // the detail-level preset supplies max_splats + sh_degree.
                let growth_stop = params
                    .get("growth_stop_iter")
                    .and_then(|v| v.as_u64())
                    .unwrap_or_else(|| (steps / 2).min(10_000));
                let max_splats = params
                    .get("max_splats")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1_500_000);
                let sh_degree = params
                    .get("sh_degree")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3)
                    .min(4);
                let export_name = output_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "output.ply".into());
                vec![
                    input_s,
                    "--total-train-iters".into(),
                    steps.to_string(),
                    "--growth-stop-iter".into(),
                    growth_stop.to_string(),
                    "--max-splats".into(),
                    max_splats.to_string(),
                    "--sh-degree".into(),
                    sh_degree.to_string(),
                    "--export-every".into(),
                    steps.to_string(),
                    "--export-path".into(),
                    workdir.to_string_lossy().into_owned(),
                    "--export-name".into(),
                    export_name,
                ]
            }
            ReconstructorKind::Msplat => {
                // msplat (native Metal): dataset positional; -n iters, -o output
                // .ply, --sh-degree. It inits from a points3D.ply next to
                // transforms.json (written by the seed step; its own reader, no
                // COLMAP binary). No splat cap — msplat bounds its own count via
                // densification thresholds.
                let steps = params
                    .get("steps")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30000);
                let sh_degree = params
                    .get("sh_degree")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3)
                    .min(4);
                vec![
                    input_s,
                    "-n".into(),
                    steps.to_string(),
                    "--sh-degree".into(),
                    sh_degree.to_string(),
                    "-o".into(),
                    output_s,
                ]
            }
            ReconstructorKind::Nerfstudio => {
                // ns-train splatfacto --data <input> ... (the export step is a
                // separate ns-export call on a real node; the trainer writes the
                // checkpoint dir we point the output at).
                let mut a = vec![
                    "splatfacto".into(),
                    "--data".into(),
                    input_s,
                    "--output-dir".into(),
                    output_s,
                ];
                if let Some(steps) = params.get("steps").and_then(|v| v.as_u64()) {
                    a.push("--max-num-iterations".into());
                    a.push(steps.to_string());
                }
                a
            }
            ReconstructorKind::Colmap => vec![
                "automatic_reconstructor".into(),
                "--image_path".into(),
                input_s,
                "--workspace_path".into(),
                output_s,
            ],
        };

        ReconstructCommand {
            program: self.kind.program().to_string(),
            args,
            workdir,
            output_path,
            output_uri,
        }
    }
}

impl Reconstructor for CliReconstructor {
    fn name(&self) -> &str {
        self.kind.name()
    }

    fn reconstruct(
        &self,
        dataset: &Dataset,
        params: &serde_json::Value,
    ) -> Result<ReconstructOutput, ComputeError> {
        let cmd = self.command(dataset, params);
        std::fs::create_dir_all(&cmd.workdir).map_err(|e| ComputeError::Backend {
            backend: self.kind.name().into(),
            message: format!("create workdir: {e}"),
        })?;

        let output = Command::new(&cmd.program)
            .args(&cmd.args)
            .current_dir(&cmd.workdir)
            .output()
            .map_err(|e| ComputeError::Backend {
                backend: self.kind.name().into(),
                message: format!("spawn {}: {e}", cmd.program),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ComputeError::Backend {
                backend: self.kind.name().into(),
                message: format!(
                    "{} exited {}: {}",
                    cmd.program,
                    output.status,
                    stderr.trim()
                ),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(ReconstructOutput {
            kind: self.kind.artifact_kind().into(),
            uri: cmd.output_uri,
            gaussian_count: parse_gaussian_count(&stdout),
            // The concrete tool that produced this (brush / msplat / nerfstudio /
            // colmap) so the client can tell a real reconstruction from the mock
            // placeholder. Reuses ReconstructorKind::name(), never hardcoded.
            backend: self.kind.name().into(),
        })
    }
}

/// Build a `file://` URI from an absolute filesystem path, percent-encoding the
/// few characters that break a URI path (space, `#`, `?`, `%`). The compute node
/// may run on a Mac whose work root has a space, so a raw concat would produce a
/// malformed URI a viewer (or the next pipeline stage) cannot resolve.
pub fn path_to_file_uri(path: &str) -> String {
    let mut out = String::from("file://");
    for ch in path.chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '%' => out.push_str("%25"),
            c => out.push(c),
        }
    }
    out
}

/// Resolve a `file://` URI (or a bare path) back to a filesystem path, reversing
/// the percent-encoding [`path_to_file_uri`] applies. A string without the
/// scheme is treated as a raw path. Only the ASCII escapes the encoder emits are
/// decoded; any other byte passes through.
pub fn file_uri_to_path(uri: &str) -> PathBuf {
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    if !path.contains('%') {
        return PathBuf::from(path);
    }
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(b) = u8::from_str_radix(&hex, 16) {
                    out.push(b as char);
                    continue;
                }
            }
            out.push('%');
            out.push_str(&hex);
        } else {
            out.push(c);
        }
    }
    PathBuf::from(out)
}

/// Pull a gaussian count out of a splat trainer's stdout. The trainers print a
/// final line like `gaussians: 248123` or `Num splats: 248123`; parse the last
/// match so the descriptor carries a real count when the tool reports one, 0
/// otherwise (a non-splat artifact, or a tool that does not print it).
pub fn parse_gaussian_count(stdout: &str) -> u64 {
    let mut count = 0u64;
    for line in stdout.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(idx) = lower
            .find("gaussians")
            .or_else(|| lower.find("num splats"))
            .or_else(|| lower.find("splats:"))
        {
            // Take the FIRST contiguous run of digits after the marker. A
            // trainer line often carries a second number (e.g. `gaussians:
            // 248123 of 1000000` or `... in 30000 steps`); concatenating every
            // digit would report a wrong/overflowing count.
            let run: String = line[idx..]
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = run.parse::<u64>() {
                count = n;
            }
        }
    }
    count
}

/// True when `program` resolves on `PATH`. Used so a reconstruct job picks a
/// real tool only when it is actually installed, and falls back to the mock
/// otherwise (CI, a node with no GPU).
pub fn is_tool_available(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(program);
        is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// True on an Apple-Silicon Mac, where the native-Metal `msplat` trainer is the
/// fast path. A compile-time fact (no runtime probe), matching the platform idiom
/// in `gpu.rs`.
pub fn is_apple_silicon() -> bool {
    cfg!(target_os = "macos") && cfg!(target_arch = "aarch64")
}

/// The seamless reconstruction path: write a native point-cloud seed co-framed
/// with the manifest's poses (no COLMAP binary), then run the native-Metal
/// `msplat` trainer initialized from it. When the seed can't be written (no
/// manifest / too few poses) it falls back to the portable random-init trainer
/// (Brush), which trains from the poses alone; with neither trainer installed it
/// falls back to the mock (CI / no-GPU). Whichever tool actually ran is stamped on
/// the output (`msplat` / `brush` / `mock`), so the honesty badge (Rule 44) is
/// always accurate — this wrapper's own name is never the backend.
pub struct SeededSplatReconstructor {
    work_root: PathBuf,
}

impl SeededSplatReconstructor {
    /// A seeded-splat reconstructor writing artifacts (and the seed scratch) under
    /// `work_root`.
    pub fn new(work_root: impl Into<PathBuf>) -> Self {
        Self {
            work_root: work_root.into(),
        }
    }

    /// The dataset directory holding `transforms.json` + `images/`: the chained
    /// stage input (`params.input_uri`) when present, else the dataset's
    /// `input_path`. `None` when neither is set (a seed cannot run).
    fn input_dir(dataset: &Dataset, params: &serde_json::Value) -> Option<PathBuf> {
        params
            .get("input_uri")
            .and_then(|v| v.as_str())
            .map(file_uri_to_path)
            .or_else(|| {
                dataset
                    .meta
                    .get("input_path")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
            })
    }

    /// Write a native point-cloud seed next to `dir`'s manifest; true when a usable
    /// cloud now sits there. Every negative outcome (no manifest, too few poses) is
    /// logged and yields false, so the caller trains with the portable random-init
    /// trainer (Brush) instead.
    fn try_seed(dir: &Path, params: &serde_json::Value) -> bool {
        match crate::seed::seed_points(dir, params) {
            Ok(n) if n >= crate::seed::MIN_SEED_POINTS => {
                tracing::info!(points = n, "reconstruct_seeded: native point cloud");
                true
            }
            Ok(n) => {
                tracing::warn!(points = n, "reconstruct_seed_sparse: falling back to brush");
                false
            }
            Err(e) => {
                tracing::warn!(error = %e, "reconstruct_seed_failed: falling back to brush");
                false
            }
        }
    }
}

impl Reconstructor for SeededSplatReconstructor {
    fn name(&self) -> &str {
        "seeded-splat"
    }

    fn reconstruct(
        &self,
        dataset: &Dataset,
        params: &serde_json::Value,
    ) -> Result<ReconstructOutput, ComputeError> {
        let seeded = Self::input_dir(dataset, params)
            .map(|dir| Self::try_seed(&dir, params))
            .unwrap_or(false);
        if seeded && is_tool_available(ReconstructorKind::Msplat.program()) {
            return CliReconstructor::new(ReconstructorKind::Msplat, &self.work_root)
                .reconstruct(dataset, params);
        }
        if is_tool_available(ReconstructorKind::Brush.program()) {
            return CliReconstructor::new(ReconstructorKind::Brush, &self.work_root)
                .reconstruct(dataset, params);
        }
        MockReconstructor.reconstruct(dataset, params)
    }
}

/// Pick a reconstructor for a job. The default hint is `auto` (also used when a
/// job carries no `backend`): on Apple Silicon with `msplat` installed it returns
/// the seamless [`SeededSplatReconstructor`] (COLMAP seed → msplat, Brush
/// fallback); else it uses `ns-train` (CUDA) or Brush when installed, falling back
/// to the [`MockReconstructor`] (CI / no-GPU). An explicit `backend` hint pins the
/// tool: `msplat` still runs through the seeded path (it needs points), any other
/// installed tool runs directly, and an unknown or uninstalled tool falls back to
/// the mock. `work_root` is where a real backend writes artifacts + the seed
/// scratch.
pub fn select_reconstructor(
    params: &serde_json::Value,
    work_root: impl Into<PathBuf>,
) -> Arc<dyn Reconstructor> {
    let work_root = work_root.into();
    let hint = params
        .get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("auto");
    if hint == "auto" {
        return auto_reconstructor(work_root);
    }
    match ReconstructorKind::from_hint(hint) {
        // An explicit msplat pin still seeds (msplat cannot init without points).
        Some(ReconstructorKind::Msplat) => Arc::new(SeededSplatReconstructor::new(work_root)),
        Some(kind) if is_tool_available(kind.program()) => {
            Arc::new(CliReconstructor::new(kind, work_root))
        }
        // A pinned-but-uninstalled tool, or an unknown hint, falls back to the mock
        // so the job still completes deterministically (CI / no-GPU).
        _ => Arc::new(MockReconstructor),
    }
}

/// The `auto` backend policy: prefer the native-Metal seeded-splat path on Apple
/// Silicon, then a CUDA trainer, then the portable trainer, then the mock.
fn auto_reconstructor(work_root: PathBuf) -> Arc<dyn Reconstructor> {
    if is_apple_silicon() && is_tool_available(ReconstructorKind::Msplat.program()) {
        return Arc::new(SeededSplatReconstructor::new(work_root));
    }
    if is_tool_available(ReconstructorKind::Nerfstudio.program()) {
        return Arc::new(CliReconstructor::new(
            ReconstructorKind::Nerfstudio,
            work_root,
        ));
    }
    if is_tool_available(ReconstructorKind::Brush.program()) {
        return Arc::new(CliReconstructor::new(ReconstructorKind::Brush, work_root));
    }
    Arc::new(MockReconstructor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dataset(id: &str, input: Option<&str>) -> Dataset {
        let meta = match input {
            Some(p) => serde_json::json!({ "input_path": p }),
            None => serde_json::json!({}),
        };
        Dataset {
            id: id.into(),
            kind: "bag".into(),
            created_ms: 0,
            meta,
        }
    }

    #[test]
    fn kind_from_hint_maps_known_tools() {
        assert_eq!(
            ReconstructorKind::from_hint("brush"),
            Some(ReconstructorKind::Brush)
        );
        assert_eq!(
            ReconstructorKind::from_hint("SPLATFACTO"),
            Some(ReconstructorKind::Nerfstudio)
        );
        assert_eq!(
            ReconstructorKind::from_hint("colmap"),
            Some(ReconstructorKind::Colmap)
        );
        assert_eq!(
            ReconstructorKind::from_hint("msplat"),
            Some(ReconstructorKind::Msplat)
        );
        assert_eq!(ReconstructorKind::from_hint("nope"), None);
    }

    #[test]
    fn brush_command_includes_input_export_and_steps() {
        let r = CliReconstructor::new(ReconstructorKind::Brush, "/work");
        let cmd = r.command(
            &dataset("ds-1", Some("/data/ds-1")),
            &serde_json::json!({ "steps": 30000 }),
        );
        assert_eq!(cmd.program, "brush");
        assert_eq!(cmd.workdir, Path::new("/work/ds-1"));
        assert_eq!(cmd.output_path, Path::new("/work/ds-1/output.ply"));
        assert_eq!(cmd.output_uri, "file:///work/ds-1/output.ply");
        assert!(cmd.args.contains(&"/data/ds-1".to_string()));
        assert!(cmd.args.contains(&"--total-train-iters".to_string()));
        assert!(cmd.args.contains(&"--export-every".to_string()));
        assert!(cmd.args.contains(&"--export-path".to_string()));
        // export to the absolute workdir with a literal filename so the .ply
        // lands exactly at output_path.
        assert!(cmd.args.contains(&"/work/ds-1".to_string()));
        assert!(cmd.args.contains(&"--export-name".to_string()));
        assert!(cmd.args.contains(&"output.ply".to_string()));
        assert!(cmd.args.contains(&"30000".to_string()));
        // Densification must be bounded (else a small scene grows to millions of
        // gaussians and each step slows to a crawl).
        assert!(cmd.args.contains(&"--growth-stop-iter".to_string()));
        assert!(cmd.args.contains(&"--max-splats".to_string()));
        assert!(cmd.args.contains(&"--sh-degree".to_string()));
    }

    /// Value that follows `flag` in an arg vec, if present.
    fn arg_val(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    }

    #[test]
    fn brush_command_bounds_densification_and_honors_overrides() {
        let r = CliReconstructor::new(ReconstructorKind::Brush, "/work");

        // Defaults derived from steps: growth stops at steps/2 (capped 10k), a real
        // splat cap, sh-degree 3.
        let cmd = r.command(
            &dataset("ds", Some("/d")),
            &serde_json::json!({ "steps": 7000 }),
        );
        assert_eq!(
            arg_val(&cmd.args, "--growth-stop-iter").as_deref(),
            Some("3500")
        );
        assert_eq!(
            arg_val(&cmd.args, "--max-splats").as_deref(),
            Some("1500000")
        );
        assert_eq!(arg_val(&cmd.args, "--sh-degree").as_deref(), Some("3"));

        // growth-stop is capped at 10k for long runs.
        let cmd = r.command(
            &dataset("ds", Some("/d")),
            &serde_json::json!({ "steps": 30000 }),
        );
        assert_eq!(
            arg_val(&cmd.args, "--growth-stop-iter").as_deref(),
            Some("10000")
        );

        // A bare job (no steps) is still bounded (the bug was an unbounded default).
        let cmd = r.command(&dataset("ds", Some("/d")), &serde_json::json!({}));
        assert!(cmd.args.contains(&"--max-splats".to_string()));
        assert!(cmd.args.contains(&"--growth-stop-iter".to_string()));

        // Per-job overrides win (the detail-level preset drives these).
        let cmd = r.command(
            &dataset("ds", Some("/d")),
            &serde_json::json!({ "steps": 15000, "max_splats": 600000, "sh_degree": 2, "growth_stop_iter": 5000 }),
        );
        assert_eq!(
            arg_val(&cmd.args, "--max-splats").as_deref(),
            Some("600000")
        );
        assert_eq!(arg_val(&cmd.args, "--sh-degree").as_deref(), Some("2"));
        assert_eq!(
            arg_val(&cmd.args, "--growth-stop-iter").as_deref(),
            Some("5000")
        );
    }

    #[test]
    fn msplat_command_maps_dataset_steps_and_sh() {
        let r = CliReconstructor::new(ReconstructorKind::Msplat, "/work");
        let cmd = r.command(
            &dataset("ds", Some("/colmap")),
            &serde_json::json!({ "steps": 7000, "sh_degree": 2 }),
        );
        assert_eq!(cmd.program, "msplat");
        assert_eq!(cmd.output_path, Path::new("/work/ds/output.ply"));
        // COLMAP dataset dir is the positional input (msplat inits from its points).
        assert_eq!(cmd.args.first().map(|s| s.as_str()), Some("/colmap"));
        assert_eq!(arg_val(&cmd.args, "-n").as_deref(), Some("7000"));
        assert_eq!(arg_val(&cmd.args, "--sh-degree").as_deref(), Some("2"));
        assert_eq!(
            arg_val(&cmd.args, "-o").as_deref(),
            Some("/work/ds/output.ply")
        );
    }

    #[test]
    fn input_defaults_under_workdir_when_meta_omits_it() {
        let r = CliReconstructor::new(ReconstructorKind::Colmap, "/work");
        let cmd = r.command(&dataset("ds-2", None), &serde_json::json!({}));
        assert_eq!(cmd.program, "colmap");
        // input defaults to <work>/<id>/input
        assert!(cmd.args.iter().any(|a| a == "/work/ds-2/input"));
        assert!(cmd.args.iter().any(|a| a == "automatic_reconstructor"));
        // COLMAP's output is its workspace directory, not a single .ply file.
        assert_eq!(cmd.output_path, Path::new("/work/ds-2/colmap"));
        assert!(cmd.args.iter().any(|a| a == "/work/ds-2/colmap"));
    }

    #[test]
    fn pipeline_input_uri_overrides_the_dataset_input() {
        // A chained stage consumes the prior stage's output (input_uri) instead
        // of the dataset's own input, so the COLMAP->train->... chain is wired.
        let r = CliReconstructor::new(ReconstructorKind::Brush, "/work");
        let cmd = r.command(
            &dataset("ds-1", Some("/data/original")),
            &serde_json::json!({ "input_uri": "file:///work/ds-1/colmap" }),
        );
        // the train stage trains on the COLMAP workspace, not the raw dataset
        assert!(cmd.args.iter().any(|a| a == "/work/ds-1/colmap"));
        assert!(!cmd.args.iter().any(|a| a == "/data/original"));
    }

    #[test]
    fn nerfstudio_command_shape() {
        let ns = CliReconstructor::new(ReconstructorKind::Nerfstudio, "/w");
        let nc = ns.command(
            &dataset("d", Some("/imgs")),
            &serde_json::json!({ "steps": 7000 }),
        );
        assert_eq!(nc.program, "ns-train");
        assert_eq!(nc.args.first().map(String::as_str), Some("splatfacto"));
        assert!(nc.args.iter().any(|a| a == "--max-num-iterations"));
        assert!(nc.args.iter().any(|a| a == "7000"));
    }

    #[test]
    fn gaussian_count_parses_the_first_run_only() {
        assert_eq!(
            parse_gaussian_count("training...\ngaussians: 248123\n"),
            248123
        );
        assert_eq!(
            parse_gaussian_count("Num splats: 1000\nNum splats: 2500"),
            2500
        );
        assert_eq!(parse_gaussian_count("splats: 42 done"), 42);
        assert_eq!(parse_gaussian_count("no count here"), 0);
        // A trailing number on the count line must NOT be concatenated.
        assert_eq!(parse_gaussian_count("gaussians: 248123 of 1000000"), 248123);
        assert_eq!(
            parse_gaussian_count("Num splats: 248123 (step 30000)"),
            248123
        );
    }

    #[test]
    fn select_falls_back_to_mock_for_an_unknown_or_uninstalled_backend() {
        // An unknown backend hint can never resolve a tool, so it always falls back
        // to the mock regardless of what is on PATH (PATH-independent). (An absent
        // hint resolves to `auto`, whose result depends on what is installed, so it
        // is exercised by the daemon path + P6, not this pure PATH-independent test.)
        let r = select_reconstructor(&serde_json::json!({ "backend": "no-such-tool" }), "/work");
        assert_eq!(r.name(), "mock");
    }

    #[test]
    fn an_explicit_msplat_hint_routes_through_the_seeded_path() {
        // Pinning msplat always selects the seeded-splat reconstructor (msplat needs
        // a point cloud to initialize); the seed-vs-fallback decision happens at run
        // time, so this is PATH-independent.
        let r = select_reconstructor(&serde_json::json!({ "backend": "msplat" }), "/work");
        assert_eq!(r.name(), "seeded-splat");
    }

    #[test]
    fn seeded_input_dir_prefers_input_uri_then_meta_then_none() {
        // The chained stage input (input_uri) wins; else the dataset's input_path;
        // else None (nothing to seed → the caller trains from poses).
        let d = dataset("ds", Some("/data/ds"));
        assert_eq!(
            SeededSplatReconstructor::input_dir(
                &d,
                &serde_json::json!({ "input_uri": "file:///w/ds/colmap" })
            ),
            Some(PathBuf::from("/w/ds/colmap"))
        );
        assert_eq!(
            SeededSplatReconstructor::input_dir(&d, &serde_json::json!({})),
            Some(PathBuf::from("/data/ds"))
        );
        assert_eq!(
            SeededSplatReconstructor::input_dir(&dataset("ds", None), &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn is_tool_available_finds_a_real_binary_and_rejects_a_bogus_one() {
        // `sh` is on PATH on every unix CI host; a bogus name is not. This
        // exercises the PATH walk + the exec-bit check both ways, so an inverted
        // check (which would route every job to the mock) fails CI.
        #[cfg(unix)]
        assert!(is_tool_available("sh"));
        assert!(!is_tool_available("ados-definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn file_uri_round_trips_a_spaced_path() {
        let uri = path_to_file_uri("/My Work/ds 1/output.ply");
        assert_eq!(uri, "file:///My%20Work/ds%201/output.ply");
        assert_eq!(
            file_uri_to_path(&uri),
            Path::new("/My Work/ds 1/output.ply")
        );
        // A bare path (no scheme) resolves unchanged.
        assert_eq!(file_uri_to_path("/plain/path"), Path::new("/plain/path"));
    }

    #[test]
    fn selecting_reconstructor_falls_back_to_mock_with_no_real_backend() {
        // With a backend hint that can never resolve a tool, the selector routes
        // to the mock and produces its deterministic artifact, so the queue runs
        // end to end on a node with no real backend (CI / no-GPU).
        let r = SelectingReconstructor::new("/work");
        let out = r
            .reconstruct(
                &dataset("ds-9", Some("/data/ds-9")),
                &serde_json::json!({ "backend": "no-such-tool" }),
            )
            .unwrap();
        assert_eq!(out.kind, "splat");
        assert_eq!(out.uri, "mock://splat/ds-9");
    }

    #[test]
    fn missing_program_yields_a_backend_error_not_a_panic() {
        // A CliReconstructor for a tool that does not exist returns a Backend
        // error from reconstruct() (spawn failure), never panics.
        let r = CliReconstructor::new(
            ReconstructorKind::Brush,
            std::env::temp_dir().join("ados-compute-test"),
        );
        let err = r
            .reconstruct(&dataset("ds-x", None), &serde_json::json!({}))
            .unwrap_err();
        match err {
            ComputeError::Backend { backend, .. } => assert_eq!(backend, "brush"),
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    #[test]
    fn real_reconstructor_kinds_expose_their_backend_names() {
        // The `backend` a real reconstruction stamps onto its ReconstructOutput is
        // exactly `ReconstructorKind::name()` (see CliReconstructor::reconstruct):
        // a real world model NEVER carries `mock`, it carries its tool name, so a
        // client can tell a placeholder from the real thing.
        for (kind, name) in [
            (ReconstructorKind::Brush, "brush"),
            (ReconstructorKind::Msplat, "msplat"),
            (ReconstructorKind::Nerfstudio, "nerfstudio"),
            (ReconstructorKind::Colmap, "colmap"),
        ] {
            assert_eq!(kind.name(), name);
            // The Reconstructor::name() the CliReconstructor reports is the same
            // string it stamps into `ReconstructOutput.backend`.
            assert_eq!(CliReconstructor::new(kind, "/w").name(), name);
            // And none of them is the placeholder marker.
            assert_ne!(kind.name(), "mock");
        }
    }

    #[test]
    fn real_backend_stamps_its_kind_on_the_output() {
        // Exercise the success path of a REAL CliReconstructor end to end so the
        // Ok(...) branch that stamps `backend` is covered, without depending on a
        // heavyweight GPU tool: drop a trivial fake executable named after the
        // kind's program onto a temp dir prepended to PATH, run, and assert the
        // output carries the concrete backend name (not `mock`).
        //
        // We use `colmap` because no other test relies on it being absent
        // (missing_program uses `brush`; is_tool_available checks `sh`/a bogus
        // name; the selection tests are pure and never probe `colmap` on the real
        // PATH), and we PREPEND (not replace) PATH and restore it BEFORE asserting
        // so a panic can never leak a mutated PATH into a parallel test. std's
        // internal env lock serializes this against other tests' env reads; the
        // crate already mutates env in tests (heartbeat_sidecar).
        let kind = ReconstructorKind::Colmap;
        let base =
            std::env::temp_dir().join(format!("ados-compute-real-backend-{}", std::process::id()));
        let bin_dir = base.join("bin");
        let work_root = base.join("work");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&work_root).unwrap();

        // A fake tool that just exits 0. reconstruct() checks only the exit status
        // and parses stdout; it does not require the output file to exist.
        let fake = bin_dir.join(kind.program());
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let old_path = std::env::var_os("PATH");
        let mut search = vec![bin_dir.clone()];
        if let Some(p) = &old_path {
            search.extend(std::env::split_paths(p));
        }
        std::env::set_var("PATH", std::env::join_paths(&search).unwrap());

        let r = CliReconstructor::new(kind, &work_root);
        let result = r.reconstruct(&dataset("ds-real", None), &serde_json::json!({}));

        // Restore PATH BEFORE asserting so a failure never leaves it mutated.
        match old_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        let _ = std::fs::remove_dir_all(&base);

        let out = result.expect("fake colmap exits 0, reconstruct should succeed");
        assert_eq!(out.backend, "colmap");
        assert_ne!(out.backend, "mock");
        assert_eq!(out.kind, "pointcloud");
    }
}
