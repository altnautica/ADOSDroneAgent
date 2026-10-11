//! The shared plugin-manifest conformance corpus, run through the install
//! path's checks. The same fixtures run through the schema-source model in
//! `tests/test_manifest_corpus.py`, so the two validators accept and refuse
//! the same manifests.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ados_plugin_host::services::declared_services;
use ados_plugin_host::PluginManifest;

/// What the install path checks before it writes anything: the manifest
/// parse, then the declared services (validated at install, not at parse,
/// so a plugin with a bad service still parses for remove and listing).
fn install_check(text: &str) -> Result<PluginManifest, String> {
    let manifest = PluginManifest::from_yaml_text(text).map_err(|e| e.0)?;
    declared_services(&manifest).map_err(|e| e.0)?;
    Ok(manifest)
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/fixtures/plugin-manifest")
}

/// The `.yaml` fixtures in `dir`, by file stem, sorted.
fn fixtures(dir: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        .map(|p| {
            let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&p).unwrap();
            (stem, text)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn every_valid_fixture_parses() {
    let valid = fixtures(&corpus_dir().join("valid"));
    assert!(!valid.is_empty());
    for (name, text) in valid {
        if let Err(e) = install_check(&text) {
            panic!("valid fixture {name} was refused: {e}");
        }
    }
}

#[test]
fn every_invalid_fixture_is_refused() {
    let invalid = fixtures(&corpus_dir().join("invalid"));
    assert!(!invalid.is_empty());
    for (name, text) in invalid {
        assert!(
            install_check(&text).is_err(),
            "invalid fixture {name} was accepted"
        );
    }
}

#[test]
fn every_invalid_fixture_names_its_rule() {
    let dir = corpus_dir().join("invalid");
    let expected = std::fs::read_to_string(dir.join("expected.toml")).unwrap();
    let listed: BTreeSet<String> = expected
        .lines()
        .filter_map(|l| l.trim().strip_prefix('[')?.strip_suffix(']'))
        .map(str::to_string)
        .collect();
    let present: BTreeSet<String> = fixtures(&dir).into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        listed, present,
        "expected.toml and the invalid fixtures disagree"
    );
}

#[test]
fn the_published_extension_manifests_parse() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../ADOSExtensions/extensions");
    let Ok(entries) = std::fs::read_dir(&root) else {
        eprintln!(
            "skipped: no extensions checkout beside this repo at {}",
            root.display()
        );
        return;
    };
    let mut parsed = 0;
    for entry in entries.flatten() {
        let manifest = entry.path().join("manifest.yaml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        if let Err(e) = install_check(&text) {
            panic!("{} was refused: {e}", manifest.display());
        }
        parsed += 1;
    }
    assert!(
        parsed > 0,
        "no extension manifest found under {}",
        root.display()
    );
}
