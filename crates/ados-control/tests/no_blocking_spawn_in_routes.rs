//! The route layer may not spawn a process synchronously.
//!
//! Every route in this crate is a thin `async fn` handler over a sync helper
//! that holds the I/O. That shape is fine, and it is exactly why a dozen
//! `std::process::Command::…output()` calls ended up one or two sync frames
//! below an axum handler, running on the reactor: the helper is sync, so
//! nothing in the type system objects. Each one was introduced by someone
//! following the file's existing pattern.
//!
//! A pattern that reintroduces a defect needs a boundary, not a code review.
//! So this is a boundary test rather than a behavioural one on purpose: the
//! property is "no call site of this kind exists in this layer", which is a
//! statement about the layer, not about any one handler's output. The
//! behavioural proof that the replacement is correct — that a hung command is
//! bounded and its child reaped, which `spawn_blocking` does not give — lives
//! in `crate::probe`'s own tests.
//!
//! The replacement is `crate::probe`, which pairs `tokio::process` with
//! `tokio::time::timeout` and `kill_on_drop`. Its probes are `async fn`, so a
//! sync helper cannot call one without becoming async itself.

use std::path::{Path, PathBuf};

/// `std::process::Command`, however it is spelled. A file that imports
/// `std::process::Command` and then writes bare `Command::new` is the common
/// case, so the import itself counts.
const BLOCKING_SPAWN_MARKERS: &[&str] = &["std::process::Command", "use std::process::Command"];

/// `crate::probe` is the sanctioned seam and is allowed to name the async
/// `Command` (it does not use the blocking one). Nothing else is exempt.
fn is_exempt(path: &Path) -> bool {
    matches!(path.file_name().and_then(|n| n.to_str()), Some("probe.rs"))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Strip `#[cfg(test)]` module bodies and doc comments so a test helper or an
/// explanatory comment naming the old idiom is not a violation. Crude but
/// sufficient: the check is a tripwire, and a false positive is cheap to
/// resolve by moving the mention into a doc comment.
fn code_lines(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut in_test_mod = false;
    let mut test_mod_depth: i32 = 0;
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") {
            in_test_mod = true;
            test_mod_depth = 0;
            continue;
        }
        if in_test_mod {
            test_mod_depth += line.matches('{').count() as i32;
            test_mod_depth -= line.matches('}').count() as i32;
            if test_mod_depth <= 0 && line.contains('}') {
                in_test_mod = false;
            }
            continue;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        out.push((idx + 1, line));
    }
    out
}

#[test]
fn the_route_layer_never_spawns_a_process_synchronously() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(
        files.len() > 50,
        "expected to scan the whole route layer, found {} files under {}",
        files.len(),
        root.display()
    );

    let mut violations: Vec<String> = Vec::new();
    for path in &files {
        if is_exempt(path) {
            continue;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for (lineno, line) in code_lines(&text) {
            if BLOCKING_SPAWN_MARKERS.iter().any(|m| line.contains(m)) {
                let rel = path.strip_prefix(&root).unwrap_or(path);
                violations.push(format!("{}:{}: {}", rel.display(), lineno, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "{} blocking process spawn(s) in the route layer. Each one stalls a \
         reactor worker for the life of the command, and is unbounded — a \
         wedged `nmcli`/`bluetoothctl` never returns. Route them through \
         `crate::probe` (tokio::process + timeout + kill_on_drop), whose \
         probes are `async fn` so a sync helper cannot call them.\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}

/// The other half of the rule. `spawn_blocking` around a process wait relocates
/// an unbounded hang onto the blocking pool instead of bounding it: the pool
/// thread is gone for the life of the process, and with ~512 of them the
/// failure is slow and invisible rather than loud. So the fix for a blocking
/// spawn is never `spawn_blocking(|| Command::new(..))`.
#[test]
fn no_route_wraps_a_blocking_spawn_in_spawn_blocking() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes");
    let mut files = Vec::new();
    rust_files(&root, &mut files);

    let mut violations: Vec<String> = Vec::new();
    for path in &files {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !line.contains("spawn_blocking") {
                continue;
            }
            // Look ahead a few lines for a process spawn inside the closure.
            let window = lines[i..(i + 8).min(lines.len())].join("\n");
            if window.contains("Command::new") {
                let rel = path.strip_prefix(&root).unwrap_or(path);
                violations.push(format!("{}:{}", rel.display(), i + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "spawn_blocking around a process spawn moves an unbounded hang to the \
         blocking pool rather than bounding it. Use `crate::probe::capture` / \
         `status_only`, which kill and reap the child on timeout.\n  {}",
        violations.join("\n  ")
    );
}
