//! Artifact checks: ELF architecture, local artifact directories, host execution probe.

use super::*;

/// The first 20 bytes of a file (an ELF identification block plus `e_machine`),
/// or an empty vector when it cannot be read. Short reads are fine: the parser
/// treats anything it cannot decode as not-an-ELF.
pub(super) fn read_header(path: &Path) -> Vec<u8> {
    use std::io::Read;
    let mut buf = [0u8; 20];
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    buf[..filled].to_vec()
}

/// The ELF `e_machine` value a binary must declare to run on `arch`, or `None`
/// for an architecture this check has no opinion about (in which case it does
/// not run at all — an unknown host is not grounds for refusing an artifact).
pub(super) fn expected_elf_machine(arch: &str) -> Option<u16> {
    match arch {
        "aarch64" => Some(0xB7),
        "x86_64" => Some(0x3E),
        "riscv64" => Some(0xF3),
        _ => None,
    }
}

/// Why `header` cannot run on `host_arch` (pure), or `None` when it can — or
/// when this check cannot say.
///
/// Decodes the ELF identification block: magic, 64-bit class, endianness, and
/// the `e_machine` at offset 18. A file that is not an ELF at all is named as
/// such, because the likeliest way to reach that on a bench is copying the Mach-O
/// the same `cargo build` produced on the developer's Mac.
pub(super) fn artifact_arch_error(header: &[u8], host_arch: &str) -> Option<String> {
    // A host architecture with no table entry: the check has no opinion and does
    // not run, rather than refusing an artifact it cannot judge.
    let want = expected_elf_machine(host_arch)?;
    if header.len() < 20 || &header[..4] != b"\x7fELF" {
        return Some(format!(
            "is not an ELF executable, so it cannot run on this {host_arch} host \
             (a macOS build of the same crate looks like this). Build it for the \
             node, e.g. cargo build --release --target {host_arch}-unknown-linux-gnu."
        ));
    }
    if header[4] != 2 {
        return Some("is a 32-bit ELF; this host runs 64-bit binaries.".to_string());
    }
    // EI_DATA: 1 = little-endian, 2 = big-endian. Every target here is LE, but
    // decode honestly rather than assume.
    let machine = match header[5] {
        1 => u16::from_le_bytes([header[18], header[19]]),
        2 => u16::from_be_bytes([header[18], header[19]]),
        _ => return Some("has an unreadable ELF data encoding.".to_string()),
    };
    if machine == want {
        return None;
    }
    Some(format!(
        "is built for ELF machine {machine:#x}, not the {host_arch} this node \
         runs ({want:#x}). Build it for the node, e.g. cargo build --release \
         --target {host_arch}-unknown-linux-gnu."
    ))
}

/// Names in an artifacts directory this install has no use for: not a sidecar,
/// not a catalog artifact.
///
/// Reported, never fatal, and deliberately so. The natural thing to point the
/// flag at is a `target/release` directory, which legitimately holds `*.d` dep
/// files, `lib*.rlib`, and `ados-installer` itself — none of which is a service
/// this installer places. Rejecting the directory for holding them would make
/// the flag unusable for its only purpose.
///
/// What the report buys is the case that actually matters: a mistyped filename
/// would otherwise be the worst outcome available here — that service silently
/// falls back to the release, the install succeeds, and the bench measures the
/// OLD binary believing it measured the new one. Naming both lists (what was
/// taken from the directory, what was passed over) puts the typo in front of the
/// operator, and [`validate_artifacts_dir`] still hard-fails the single-file
/// typo, where nothing at all is recognised.
pub fn ignored_artifact_names(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .filter(|n| !n.ends_with(".sha256") && !n.ends_with(".minisig"))
        .filter(|n| n.as_str() != MANIFEST_ASSET)
        .filter(|n| !is_catalog_artifact_name(n))
        .cloned()
        .collect()
}

/// Whether `name` is a spelling of some catalog artifact — any profile's, plus
/// the onnx vision variant and the ONNX Runtime library, since a directory
/// assembled for one profile may legitimately carry another's binary.
pub(super) fn is_catalog_artifact_name(name: &str) -> bool {
    binaries::PREBUILT
        .iter()
        .chain([
            &binaries::PREBUILT_VISION_ONNX,
            &binaries::PREBUILT_VISION_ONNX_RUNTIME,
        ])
        .any(|b| b.asset == name || b.service == name)
}

/// The file names directly inside `dir` (no recursion), sorted.
pub(super) fn dir_entry_names(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}

/// Refuse an `--artifacts` directory that cannot mean what the operator meant,
/// and otherwise report what will be taken from it and what will be passed over.
///
/// Two refusals:
///
/// * The path is not a directory, or cannot be read — a typo, or a
///   `target/release` that was never built.
/// * It holds no catalog artifact at all. This is the single-file typo
///   (`ados-vidoe`) and the wrong-directory case, and it has to be fatal: the
///   alternative is an install that fetches every binary from the release and
///   reports success while the operator believes they installed their build.
///
/// Names it has no use for are returned, not refused — see
/// [`ignored_artifact_names`].
pub(super) fn validate_artifacts_dir(dir: &Path) -> Result<Vec<String>, String> {
    if !dir.is_dir() {
        return Err(format!(
            "--artifacts {}: not a directory. Point it at the directory holding \
             the built service binaries (e.g. target/release).",
            dir.display()
        ));
    }
    let names = dir_entry_names(dir)
        .map_err(|e| format!("--artifacts {}: cannot be read: {e}", dir.display()))?;
    if !names.iter().any(|n| is_catalog_artifact_name(n)) {
        return Err(format!(
            "--artifacts {}: holds no service binary. Each artifact is named for \
             its service (e.g. ados-video) or for its release asset (e.g. \
             ados-video-aarch64), with its .sha256 beside it. Nothing here \
             matches, so every binary would come from the release instead.",
            dir.display()
        ));
    }
    Ok(ignored_artifact_names(&names))
}

/// Whether a freshly-placed binary can actually `execve` on this host — probes
/// for the dynamic-linker rejection a glibc floor mismatch produces (the
/// CI-built onnx `ados-vision` links against whatever glibc its `ubuntu-22.04-arm`
/// build runner ships; a board running an older base image, e.g. Debian 11
/// Bullseye's glibc 2.31, rejects it at `execve` time with `GLIBC_x not found`
/// on stderr). That rejection happens before the program's own code runs, so a
/// short bounded wait distinguishes it cleanly from a binary that actually
/// starts: the loader either fails within milliseconds, or the process is
/// still alive when the deadline elapses (killed immediately — a fresh install
/// already tears down and restarts services around this point, so a killed
/// probe process is harmless). A spawn failure for any OTHER reason (missing
/// file, permission) is not what this probes for — treat it as "runs" so the
/// caller's existing error paths (the retry loop, the Hard/BestEffort gate)
/// handle it instead of this probe silently swallowing an unrelated fault.
pub(super) fn binary_execs_on_this_host(path: &Path) -> bool {
    use std::io::Read;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// How long the loader is given to reach a verdict. A rejection happens
    /// before the program's own code runs, so this only has to cover process
    /// creation — but it is deliberately generous, because the cost of being
    /// too tight is not a slow probe, it is a WRONG one: a deadline that
    /// elapses during an ordinary fork on a loaded machine reads as "runs".
    const VERDICT_DEADLINE: Duration = Duration::from_secs(2);
    /// `ETXTBSY` on both Linux and macOS.
    const ETXTBSY: i32 = 26;
    const BUSY_RETRIES: u32 = 10;
    const BUSY_BACKOFF: Duration = Duration::from_millis(20);

    // A binary written moments ago can refuse to exec with ETXTBSY while any
    // descriptor still holds it open for writing — including one this process
    // never opened, since a concurrent fork inherits every open descriptor in
    // the program. That is a timing artefact, not a loader verdict, so retry it
    // rather than reading it as "runs" (which would skip the very check this
    // function exists to perform).
    let mut spawned: Option<Child> = None;
    for _ in 0..BUSY_RETRIES {
        match Command::new(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => {
                spawned = Some(c);
                break;
            }
            Err(e) if e.raw_os_error() == Some(ETXTBSY) => {
                std::thread::sleep(BUSY_BACKOFF);
            }
            // Any other spawn failure is not what this probes for; leave it to
            // the caller's retry loop and Hard/BestEffort gate.
            Err(_) => return true,
        }
    }
    let Some(mut child) = spawned else {
        return true;
    };

    // Drain stderr on its own thread rather than after the process is reaped.
    // Reading only once `try_wait` reports an exit deadlocks whenever the
    // rejection message fills the pipe buffer: the child blocks on write, so it
    // never exits, so nothing ever drains it — and the probe then hits its
    // deadline and reports the rejected binary as runnable.
    let stderr_reader = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            // A read error keeps whatever arrived before it. An empty buffer
            // reads as "no rejection", which is the honest default: absence of
            // evidence, not evidence of rejection.
            let _ = s.read_to_string(&mut buf);
            buf
        })
    });

    let deadline = Instant::now() + VERDICT_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return true,
        }
    };

    // Either the child exited or we killed it; both close the pipe, so the
    // reader has finished and this join cannot hang.
    let stderr = stderr_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    match status {
        // Exited nonzero carrying a loader complaint: the rejection we probe for.
        Some(status) => !(!status.success() && stderr.contains("GLIBC_")),
        // Still alive at the deadline: the loader accepted it.
        None => true,
    }
}
