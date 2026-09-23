//! The reactor-side layer may not spawn a process synchronously.
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
//!
//! ## Why the scan reaches past `src/routes`
//!
//! It used to scan `src/routes/**` only, and that is the shape of hole this
//! boundary exists to close: `ados_protocol::wfb_status::regulatory_domain`
//! forked `iw reg get` with a blocking `std::process::Command`, and every
//! `GET /api/wfb`, `GET /api/status/full` and cloud-heartbeat tick called it —
//! so the defect sat one crate below a scan that reported clean. A route
//! handler's stack does not stop at the crate boundary, so neither does this.
//!
//! [`SCAN_ROOTS`] therefore names the route layer plus the library surface the
//! route layer calls into. Two of those crates are daemons in their own right
//! (`ados-video`, `ados-groundlink`), and most of their source belongs to their
//! own binary rather than to a request path, so for those the roots name the
//! exact modules `ados-control` imports.

use std::path::{Path, PathBuf};

/// Lines that name `std::process::Command`, however it is spelled: the full
/// path (`std::process::Command::new`, `use std::process::Command;`, `… as
/// Cmd`), a grouped import (`use std::process::{Command, Stdio};`, including a
/// group that spans lines), and the module-qualified form after
/// `use std::process;` (`process::Command::new`). A file that imports the type
/// and then writes bare `Command::new` is the common case, so the import itself
/// counts. `tokio::process::Command` is the replacement and never matches.
fn blocking_spawn_hits(lines: &[(usize, &str)]) -> Vec<(usize, String)> {
    let mut hits = Vec::new();
    for (i, (lineno, line)) in lines.iter().enumerate() {
        let mut hit = line.contains("std::process::Command");
        if !hit {
            if let Some(at) = line.find("std::process::{") {
                // Gather the group up to its closing brace, across lines.
                let mut group = line[at + "std::process::{".len()..].to_string();
                let mut j = i + 1;
                while !group.contains('}') && j < lines.len() {
                    group.push_str(lines[j].1);
                    j += 1;
                }
                let body = group.split('}').next().unwrap_or("");
                hit = body.split(',').any(|item| {
                    let item = item.trim();
                    item == "Command" || item.starts_with("Command ")
                });
            }
        }
        if !hit {
            hit = line.match_indices("process::Command").any(|(at, _)| {
                let before = &line[..at];
                !before.ends_with("tokio::") && !before.ends_with("std::")
            });
        }
        if hit {
            hits.push((*lineno, line.trim().to_string()));
        }
    }
    hits
}

/// The route layer, plus the library surface a route handler's stack reaches.
///
/// Paths are relative to the workspace directory (`crates/`). A whole-crate
/// entry means every file in that crate is callable from a request path; a
/// file or module entry means only that part is.
///
/// Not listed, and why: `ados-control/src` outside `routes/`.
/// `hw_local::sysctl_string` spawns `sysctl` on the macOS-only leg, reached
/// from `routes/status.rs`. Add `"ados-control/src"` here once that read is off
/// the reactor; the rest of that directory is already clean, and
/// `src/probe.rs` names the blocking idiom only inside its doc comment, which
/// this scan strips.
const SCAN_ROOTS: &[&str] = &[
    // The route layer itself.
    "ados-control/src/routes",
    // Libraries whose whole surface is reachable from a request path.
    "ados-protocol/src",
    "ados-offload/src",
    "ados-config/src",
    "ados-hid/src",
    "ados-swarmbus/src",
    "ados-macpin/src",
    // Daemon crates: only the modules `ados-control` imports.
    "ados-video/src/config.rs",
    "ados-video/src/recorder.rs",
    "ados-video/src/mediamtx.rs",
    "ados-video/src/profile",
    "ados-groundlink/src/lib.rs",
    "ados-groundlink/src/fleet.rs",
    "ados-groundlink/src/fleet_hero.rs",
    "ados-groundlink/src/fleet_identity_policy.rs",
    "ados-groundlink/src/aux_peers.rs",
    "ados-groundlink/src/paths.rs",
];

/// Files inside [`SCAN_ROOTS`] that may still name the blocking idiom, each
/// with the reason it is not a reactor hazard. Workspace-relative paths, never
/// bare file names: an exemption that matches by name alone widens itself the
/// next time someone adds a file with that name.
///
/// Every entry is asserted to still resolve to a scanned file, so an exemption
/// cannot outlive the code it was written for.
const EXEMPT: &[(&str, &str)] = &[
    (
        "ados-protocol/src/reach.rs",
        "the `hostname(1)` read is the non-Linux fallback only — every SBC \
         answers from /proc/sys/kernel/hostname and never reaches it — and it \
         is probed at most once per HOSTNAME_PROBE_TTL rather than per request",
    ),
    (
        "ados-macpin/src/engine.rs",
        "both route-reachable entry points (`apply_live`, `remove_pin_link`) \
         are async over tokio::process with a timeout and kill_on_drop; the \
         remaining sync `Command` sites are reachable only from \
         `write_pin_link` / `reconcile`, which the installer step and the \
         supervisor reconciler drive off any reactor. This scan is \
         file-granular, so the file needs naming even though the hazard is \
         gone — drop the entry if those sync legs move to the async seam",
    ),
];

/// The workspace directory (`crates/`), the root every scan path is relative to.
fn workspace_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory always has a parent")
        .to_path_buf()
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

/// Every `.rs` file under [`SCAN_ROOTS`], with the workspace-relative path each
/// violation and exemption is keyed on.
///
/// A root that resolves to nothing is a hard failure, not an empty result: a
/// renamed crate or a moved module would otherwise silently shrink the scan,
/// and a boundary test that measures nothing reports clean forever.
fn scanned_files() -> Vec<(String, PathBuf)> {
    let workspace = workspace_dir();
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for root in SCAN_ROOTS {
        let path = workspace.join(root);
        let mut found = Vec::new();
        if path.is_dir() {
            rust_files(&path, &mut found);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") && path.is_file() {
            found.push(path.clone());
        }
        assert!(
            !found.is_empty(),
            "scan root {root} resolved to no Rust file under {}. A moved or \
             renamed module silently shrinks this boundary, so fix the root \
             rather than letting the scan get smaller.",
            workspace.display()
        );
        for file in found {
            let rel = file
                .strip_prefix(&workspace)
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, file));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Strip `#[cfg(test)]` items and comments so a test helper or an explanatory
/// comment naming the old idiom is not a violation.
///
/// The brace counting starts at the annotated item, not at the attribute, and a
/// one-line item (`#[cfg(test)] use …;`, `#[cfg(test)] mod tests;`) is skipped
/// by itself. Counting from the attribute — with "leave the skip state on the
/// first line containing `}`" — swallowed the whole remainder of any file whose
/// annotated item carried no brace, which turns the scan of that file into a
/// silent no-op.
fn code_lines(text: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            i += 1;
            continue;
        }
        if !trimmed.starts_with("#[cfg(test)]") {
            out.push((i + 1, line));
            i += 1;
            continue;
        }
        i += 1;
        let mut depth: i32 = 0;
        let mut opened = false;
        while i < lines.len() {
            let body = lines[i];
            let open = body.matches('{').count() as i32;
            depth += open - body.matches('}').count() as i32;
            i += 1;
            if open > 0 {
                opened = true;
            }
            if depth <= 0 && (opened || open == 0) {
                break;
            }
        }
    }
    out
}

#[test]
fn the_reactor_side_layer_never_spawns_a_process_synchronously() {
    let files = scanned_files();
    assert!(
        files.len() > 150,
        "expected to scan the route layer and the libraries it calls into, \
         found only {} files",
        files.len()
    );

    let exempt: Vec<&str> = EXEMPT.iter().map(|(path, _)| *path).collect();
    for path in &exempt {
        assert!(
            files.iter().any(|(rel, _)| rel == path),
            "the exemption for {path} no longer resolves to a scanned file. \
             Delete it rather than leaving a stale hole in the boundary."
        );
    }

    let mut violations: Vec<String> = Vec::new();
    for (rel, path) in &files {
        if exempt.contains(&rel.as_str()) {
            continue;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for (lineno, line) in blocking_spawn_hits(&code_lines(&text)) {
            violations.push(format!("{rel}:{lineno}: {line}"));
        }
    }

    assert!(
        violations.is_empty(),
        "{} blocking process spawn(s) on a path a route handler reaches. Each \
         one stalls a reactor worker for the life of the command, and is \
         unbounded — a wedged `nmcli`/`bluetoothctl`/`iw` never returns. Route \
         them through `ados_control::probe` (tokio::process + timeout + \
         kill_on_drop), whose probes are `async fn` so a sync helper cannot \
         call them, or take the reading on a background task and serve the \
         cached value.\n  {}",
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
fn nothing_reachable_from_a_route_wraps_a_blocking_spawn_in_spawn_blocking() {
    let files = scanned_files();
    let mut violations: Vec<String> = Vec::new();
    for (rel, path) in &files {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let lines = code_lines(&text);
        for (i, (lineno, line)) in lines.iter().enumerate() {
            if !line.contains("spawn_blocking") {
                continue;
            }
            // Look ahead a few lines for a process spawn inside the closure.
            let window = lines[i..(i + 8).min(lines.len())]
                .iter()
                .map(|(_, l)| *l)
                .collect::<Vec<_>>()
                .join("\n");
            if window.contains("Command::new") {
                violations.push(format!("{rel}:{lineno}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "spawn_blocking around a process spawn moves an unbounded hang to the \
         blocking pool rather than bounding it. Use `ados_control::probe::capture` \
         / `status_only`, which kill and reap the child on timeout.\n  {}",
        violations.join("\n  ")
    );
}

/// The stripper is the part of this boundary that can silently disable it, so
/// it gets its own assertions rather than being trusted.
#[test]
fn the_test_module_stripper_does_not_swallow_the_rest_of_a_file() {
    // A `#[cfg(test)]` item with no brace used to put the scanner into a skip
    // state it never left, so every later line in the file went unread.
    let text = "#[cfg(test)]\nuse std::process::Command;\nlet a = REAL_CODE;\n";
    let kept: Vec<&str> = code_lines(text).into_iter().map(|(_, l)| l).collect();
    assert_eq!(kept, vec!["let a = REAL_CODE;"]);

    // Same for a one-line annotated item that opens and closes its own braces.
    let inline = "#[cfg(test)]\nmod t { use std::process::Command; }\nlet b = REAL_CODE;\n";
    let kept: Vec<&str> = code_lines(inline).into_iter().map(|(_, l)| l).collect();
    assert_eq!(kept, vec!["let b = REAL_CODE;"]);

    // A real test module is skipped whole, and the code after it is not.
    let module =
        "fn a() {}\n#[cfg(test)]\nmod tests {\n    use std::process::Command;\n}\nfn b() {}\n";
    let kept: Vec<&str> = code_lines(module).into_iter().map(|(_, l)| l).collect();
    assert_eq!(kept, vec!["fn a() {}", "fn b() {}"]);

    // And the marker is detected on a line the stripper keeps — the property
    // every assertion above depends on.
    let live = code_lines("use std::process::Command;\n");
    assert_eq!(blocking_spawn_hits(&live).len(), 1);
}

/// Every spelling of the blocking type is caught, and the async replacement is not.
#[test]
fn every_spelling_of_the_blocking_spawn_is_detected() {
    let caught = [
        "use std::process::Command;\n",
        "let o = std::process::Command::new(\"iw\");\n",
        "use std::process::{Command, Stdio};\n",
        "use std::process::{Stdio, Command as Cmd};\n",
        "use std::process::{\n    Stdio,\n    Command,\n};\n",
        "let o = process::Command::new(\"iw\").output();\n",
    ];
    for src in caught {
        let lines = code_lines(src);
        assert!(
            !blocking_spawn_hits(&lines).is_empty(),
            "not detected: {src:?}"
        );
    }
    let clean = [
        "use tokio::process::Command;\n",
        "use std::process::{ExitStatus, Stdio};\n",
        "let c = tokio::process::Command::new(\"iw\");\n",
        "std::process::exit(1);\n",
    ];
    for src in clean {
        let lines = code_lines(src);
        assert!(
            blocking_spawn_hits(&lines).is_empty(),
            "false positive: {src:?}"
        );
    }
}
