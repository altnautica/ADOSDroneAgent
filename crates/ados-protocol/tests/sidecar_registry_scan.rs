//! Every sidecar a crate names by literal has a registry row.
//!
//! Scans `crates/**/*.rs` for `run_dir().join(..)` calls and `/run/ados/...`
//! string literals that name a `.json` file, and asserts each file name matches a
//! `[[sidecar]]` row of `contracts.toml` (through the generated
//! `ados_protocol::paths::SIDECARS`). A sidecar added without a registry row
//! fails here, so its path, writer and freshness window cannot go untracked.

use std::path::{Path, PathBuf};

use ados_protocol::paths::SIDECARS;

/// Test-only names that deliberately point at a file no service writes.
const FIXTURE_PREFIX: &str = "does-not-exist-";

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target" && name != ".git" {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `.json` file name named after `marker` in `text`, where the name runs
/// to the next `"`.
fn names_after<'a>(text: &'a str, marker: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(marker) {
        rest = &rest[i + marker.len()..];
        if let Some(end) = rest.find('"') {
            let name = &rest[..end];
            if name.ends_with(".json") && !name.contains('/') {
                out.push(name);
            }
        }
    }
    out
}

/// Whether `name` is the file of a registered sidecar. A templated row
/// (`config-status-<service>.json`) matches any name with its prefix and suffix.
fn registered(name: &str) -> bool {
    SIDECARS.iter().any(|row| {
        let file = row.path.rsplit('/').next().unwrap_or(row.path);
        match (file.find('<'), file.find('>')) {
            (Some(open), Some(close)) => {
                let (prefix, suffix) = (&file[..open], &file[close + 1..]);
                name.len() > prefix.len() + suffix.len()
                    && name.starts_with(prefix)
                    && name.ends_with(suffix)
            }
            _ => file == name,
        }
    })
}

#[test]
fn every_named_sidecar_has_a_registry_row() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .to_path_buf();
    let mut files = Vec::new();
    rust_files(&crates, &mut files);
    assert!(
        files.len() > 100,
        "scan found too few sources under {crates:?}"
    );

    let mut missing = Vec::new();
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let names = names_after(&text, "run_dir().join(\"")
            .into_iter()
            .chain(names_after(&text, "\"/run/ados/"));
        for name in names {
            if name.starts_with(FIXTURE_PREFIX) || registered(name) {
                continue;
            }
            missing.push(format!("{}: {name}", file.display()));
        }
    }
    assert!(
        missing.is_empty(),
        "sidecars named in code without a contracts.toml [[sidecar]] row:\n{}",
        missing.join("\n")
    );
}

#[test]
fn templated_rows_match_by_prefix_and_suffix() {
    assert!(registered("config-status-ados-radio.json"));
    assert!(!registered("config-status-.json"));
    assert!(registered("wfb-stats.json"));
    assert!(!registered("not-a-sidecar.json"));
}
