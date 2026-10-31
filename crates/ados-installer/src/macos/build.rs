//! Toolchain checks and the from-source build of the workstation binaries.

use std::path::{Path, PathBuf};

use ados_term::exec;
use anyhow::{anyhow, Context, Result};

use super::identity::set_executable;
use super::{Paths, WORKSTATION_BINARIES};

/// True when running as root (euid 0). Read via `id -u` so no platform-gated libc
/// dependency is pulled into the pure-logic crate.
pub(super) fn is_root() -> bool {
    exec::run("id", &["-u"]).stdout.trim() == "0"
}

/// Ensure `cargo` is reachable (the install builds the service binaries from
/// source). Hard-fail with an actionable hint when it is missing.
pub(super) fn ensure_cargo() -> Result<()> {
    if exec::run_ok("sh", &["-c", "command -v cargo"]) {
        return Ok(());
    }
    Err(anyhow!(
        "cargo not found on PATH. Install the Rust toolchain (https://rustup.rs) \
         and re-run — the macOS workstation builds its binaries from source."
    ))
}

/// Install the system dependencies via Homebrew, best-effort. `ffmpeg` backs the
/// video paths and the extensions that decode a drone's camera stream. It is not
/// required for the node to come up, so a missing `brew` or a slow bottle only
/// degrades — it never blocks the install.
pub(super) fn install_brew_deps() {
    if !exec::run_ok("sh", &["-c", "command -v brew"]) {
        println!(
            "  deps: Homebrew not found; skipping ffmpeg \
             (install brew + `brew install ffmpeg` to enable video)"
        );
        return;
    }
    // ffmpeg is a small bottle; install it so the video pipeline works.
    if have_tool("ffmpeg") {
        println!("  deps: ffmpeg already present");
    } else {
        println!("  deps: brew install ffmpeg …");
        if !exec::run_ok("brew", &["install", "ffmpeg"]) {
            println!("  deps: ffmpeg install reported a non-zero status (continuing)");
        }
    }
}

/// Install the `ados` management CLI (a Python console script) from the source
/// tree so `ados update` / `ados status` / `ados logs` / `ados uninstall` work on
/// the workstation — the Rust node ships no `ados` command of its own. Runs on
/// every upgrade so the CLI tracks the agent. Best-effort: the daemons and the
/// GCS work without it, so a missing python3/pip or a pip failure warns loudly
/// with the manual command rather than failing the whole install.
pub(super) fn install_cli(source: &Path) {
    if !exec::run_ok("sh", &["-c", "command -v python3"]) {
        println!(
            "  cli: python3 not found — the `ados` command was not installed. \
             Install Python 3, then run: python3 -m pip install {}",
            source.display()
        );
        return;
    }
    if !exec::run_ok("python3", &["-m", "pip", "--version"]) {
        println!(
            "  cli: pip is unavailable under python3 — the `ados` command was not \
             installed. Install pip, then run: python3 -m pip install {}",
            source.display()
        );
        return;
    }
    println!("  cli: installing the ados command (pip) …");
    let src = source.to_string_lossy().to_string();
    let res = exec::run("python3", &["-m", "pip", "install", "--upgrade", &src]);
    if res.success() {
        println!("  cli: ados command ready");
    } else {
        let why = res
            .stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        println!("  cli: could not install the `ados` command automatically ({why}).");
        println!(
            "       Run it by hand: python3 -m pip install {}",
            source.display()
        );
    }
}

/// True when a tool resolves on PATH.
fn have_tool(tool: &str) -> bool {
    exec::run_ok("sh", &["-c", &format!("command -v {tool}")])
}

/// Resolve the source tree to build from: the `ADOS_SOURCE_DIR` the bootstrap
/// script exports, else a walk up from the current directory for the workspace
/// manifest (`crates/ados-installer/Cargo.toml`). Errors when neither resolves.
pub(super) fn resolve_source_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("ADOS_SOURCE_DIR") {
        let p = PathBuf::from(dir);
        if p.join("crates/ados-installer/Cargo.toml").is_file() {
            return Ok(p);
        }
    }
    let mut cur = std::env::current_dir().context("reading the current directory")?;
    loop {
        if cur.join("crates/ados-installer/Cargo.toml").is_file() {
            return Ok(cur);
        }
        // Also accept being invoked from inside the `crates` dir.
        if cur.join("ados-installer/Cargo.toml").is_file() {
            if let Some(parent) = cur.parent() {
                return Ok(parent.to_path_buf());
            }
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }
    Err(anyhow!(
        "could not locate the ADOS source tree; set ADOS_SOURCE_DIR to the repo root \
         (the dir containing crates/ados-installer/Cargo.toml)"
    ))
}

/// Build the workstation binaries from source (`cargo build --release`) and place
/// each under the per-user bin dir. Hard-fail: a broken build or a missing output
/// binary means there is no node to run.
pub(super) fn build_and_install_binaries(source: &Path, paths: &Paths) -> Result<()> {
    let manifest = source.join("crates/Cargo.toml");
    let manifest_s = manifest.to_string_lossy().to_string();

    println!(
        "  build: cargo build --release ({} crates) …",
        WORKSTATION_BINARIES.len()
    );
    let mut argv: Vec<String> = vec![
        "build".to_string(),
        "--release".to_string(),
        "--manifest-path".to_string(),
        manifest_s,
    ];
    for name in WORKSTATION_BINARIES {
        argv.push("-p".to_string());
        argv.push((*name).to_string());
    }
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    // Stream the build to the operator's terminal (a source build is slow; silent
    // would look hung). `run` captures output, so use a status-only spawn here.
    let status = std::process::Command::new("cargo")
        .args(&argv_ref)
        .status()
        .context("spawning cargo build")?;
    if !status.success() {
        return Err(anyhow!(
            "cargo build --release failed ({}); fix the build and re-run",
            status
        ));
    }

    // Resolve the release output dir (respecting CARGO_TARGET_DIR like the dev
    // node contract), then copy each binary into the per-user bin dir.
    let target_root = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| source.join("crates/target"));
    let release_dir = target_root.join("release");

    for name in WORKSTATION_BINARIES {
        let built = release_dir.join(name);
        if !built.is_file() {
            return Err(anyhow!(
                "expected built binary not found: {} (the build did not produce {name})",
                built.display()
            ));
        }
        let dest = paths.bin.join(name);
        // Atomic-ish replace: copy to a temp sibling then rename over the dest so a
        // running daemon keeps its old inode through an upgrade.
        let tmp = paths.bin.join(format!(".{name}.new"));
        std::fs::copy(&built, &tmp)
            .with_context(|| format!("copy {} -> {} failed", built.display(), tmp.display()))?;
        set_executable(&tmp);
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("install {} failed", dest.display()))?;
    }
    println!(
        "  build: installed {} binaries to {}",
        WORKSTATION_BINARIES.len(),
        paths.bin.display()
    );
    Ok(())
}

/// Parse the agent version out of the source tree's `src/ados/__init__.py`
/// (`__version__ = "x.y.z"`). This is the version of the codebase the workstation
/// binaries were built from — the honest value to record + report, since a
/// Rust-only Mac node has no installed pip package to read it from. `unknown` when
/// the file is absent or carries no parseable version.
pub(super) fn agent_version_from_source(source: &Path) -> String {
    let init = source.join("src/ados/__init__.py");
    if let Ok(body) = std::fs::read_to_string(&init) {
        for line in body.lines() {
            let t = line.trim();
            if t.starts_with("__version__") {
                if let Some(rest) = t.split_once('=').map(|(_, r)| r) {
                    let v = rest.trim().trim_matches(['"', '\'']);
                    if !v.is_empty() {
                        return v.to_string();
                    }
                }
            }
        }
    }
    "unknown".to_string()
}
