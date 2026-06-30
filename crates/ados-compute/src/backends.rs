//! Real reconstructor backends, behind the [`Reconstructor`] trait.
//!
//! Each backend is a thin integration over a third-party tool the compute node
//! shells out to: Brush (a Rust gaussian-splat trainer, Metal/Vulkan/wgpu, no
//! CUDA), nerfstudio/splatfacto (Python, CUDA or MPS), COLMAP (SfM poses +
//! sparse cloud), and WebODM (orthomosaic / dense cloud). The tool itself is not
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
    Brush,
    /// nerfstudio / splatfacto: high-quality gaussian splat (Python, CUDA/MPS).
    Nerfstudio,
    /// COLMAP: structure-from-motion poses + a sparse point cloud (the pre-pass).
    Colmap,
    /// WebODM: orthomosaic + dense cloud (photogrammetry).
    Webodm,
}

impl ReconstructorKind {
    /// Parse a backend hint (the `backend` job param) to a tool, if known.
    pub fn from_hint(hint: &str) -> Option<Self> {
        match hint.to_ascii_lowercase().as_str() {
            "brush" => Some(Self::Brush),
            "nerfstudio" | "splatfacto" => Some(Self::Nerfstudio),
            "colmap" => Some(Self::Colmap),
            "webodm" => Some(Self::Webodm),
            _ => None,
        }
    }

    /// The program invoked on `PATH`.
    pub fn program(self) -> &'static str {
        match self {
            Self::Brush => "brush",
            Self::Nerfstudio => "ns-train",
            Self::Colmap => "colmap",
            Self::Webodm => "webodm",
        }
    }

    /// A stable backend name for logs + the job result.
    pub fn name(self) -> &'static str {
        match self {
            Self::Brush => "brush",
            Self::Nerfstudio => "nerfstudio",
            Self::Colmap => "colmap",
            Self::Webodm => "webodm",
        }
    }

    /// The artifact kind this tool produces (matches the world-model topics).
    pub fn artifact_kind(self) -> &'static str {
        match self {
            Self::Brush | Self::Nerfstudio => "splat",
            Self::Colmap => "pointcloud",
            Self::Webodm => "orthomosaic",
        }
    }

    /// The output file extension the artifact lands as.
    pub fn output_ext(self) -> &'static str {
        match self {
            Self::Brush | Self::Nerfstudio => "ply",
            Self::Colmap => "ply",
            Self::Webodm => "tif",
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
                let export_name = output_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "output.ply".into());
                vec![
                    input_s,
                    "--total-train-iters".into(),
                    steps.to_string(),
                    "--export-every".into(),
                    steps.to_string(),
                    "--export-path".into(),
                    workdir.to_string_lossy().into_owned(),
                    "--export-name".into(),
                    export_name,
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
            ReconstructorKind::Webodm => {
                vec![
                    "--project-path".into(),
                    input_s,
                    "--output".into(),
                    output_s,
                ]
            }
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

/// Pick a reconstructor for a job. Reads the `backend` param hint; if it names a
/// known tool that is installed, returns a [`CliReconstructor`] for it, else
/// falls back to the [`MockReconstructor`] so the job still completes with a
/// deterministic artifact (CI, a node with no GPU). `work_root` is where a real
/// backend writes artifacts.
pub fn select_reconstructor(
    params: &serde_json::Value,
    work_root: impl Into<PathBuf>,
) -> Arc<dyn Reconstructor> {
    let hint = params.get("backend").and_then(|v| v.as_str());
    if let Some(kind) = hint.and_then(ReconstructorKind::from_hint) {
        if is_tool_available(kind.program()) {
            return Arc::new(CliReconstructor::new(kind, work_root));
        }
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
            ReconstructorKind::from_hint("webodm"),
            Some(ReconstructorKind::Webodm)
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
    fn nerfstudio_and_webodm_command_shapes() {
        let ns = CliReconstructor::new(ReconstructorKind::Nerfstudio, "/w");
        let nc = ns.command(
            &dataset("d", Some("/imgs")),
            &serde_json::json!({ "steps": 7000 }),
        );
        assert_eq!(nc.program, "ns-train");
        assert_eq!(nc.args.first().map(String::as_str), Some("splatfacto"));
        assert!(nc.args.iter().any(|a| a == "--max-num-iterations"));
        assert!(nc.args.iter().any(|a| a == "7000"));

        let wo = CliReconstructor::new(ReconstructorKind::Webodm, "/w");
        let wc = wo.command(&dataset("d", Some("/imgs")), &serde_json::json!({}));
        assert_eq!(wc.program, "webodm");
        assert_eq!(wc.output_path, Path::new("/w/d/output.tif"));
        assert_eq!(wo.kind().artifact_kind(), "orthomosaic");
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
    fn select_falls_back_to_mock_when_no_real_backend() {
        // An unknown backend hint can never resolve a tool, so it always falls
        // back to the mock regardless of what is on PATH (PATH-independent).
        let r = select_reconstructor(&serde_json::json!({ "backend": "no-such-tool" }), "/work");
        assert_eq!(r.name(), "mock");
        // No hint -> mock.
        let r2 = select_reconstructor(&serde_json::json!({}), "/work");
        assert_eq!(r2.name(), "mock");
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
}
