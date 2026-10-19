//! The vendored capture contract stays byte-identical to its pinned hashes,
//! and to the upstream tree when a sibling checkout of it is present.

use std::collections::BTreeMap;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

#[derive(serde::Deserialize)]
struct Vendored {
    upstream_path: String,
    files: BTreeMap<String, String>,
}

fn contract_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("contract")
}

fn vendored() -> Vendored {
    let text = std::fs::read_to_string(contract_dir().join("VENDORED.toml")).unwrap();
    toml::from_str(&text).unwrap()
}

#[test]
fn vendored_contract_is_byte_identical() {
    let v = vendored();
    assert!(!v.files.is_empty());
    for (rel, want) in &v.files {
        let bytes = std::fs::read(contract_dir().join(rel))
            .unwrap_or_else(|e| panic!("{rel}: {e}"));
        let got = hex::encode(Sha256::digest(&bytes));
        assert_eq!(&got, want, "{rel} drifted from VENDORED.toml");
    }
    // Every file under contract/ (but the manifest itself) is pinned.
    let mut stack = vec![contract_dir()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let rel = p
                .strip_prefix(contract_dir())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel != "VENDORED.toml" {
                assert!(v.files.contains_key(&rel), "{rel} is not pinned");
            }
        }
    }
}

/// When the ADOSExtensions checkout sits beside this repository (the master
/// workspace layout), the copy must equal upstream byte for byte.
#[test]
fn vendored_contract_matches_upstream_when_present() {
    let v = vendored();
    let upstream = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../ADOSExtensions")
        .join(&v.upstream_path);
    if !upstream.is_dir() {
        return;
    }
    for rel in v.files.keys() {
        let ours = std::fs::read(contract_dir().join(rel)).unwrap();
        let theirs = std::fs::read(upstream.join(rel)).unwrap();
        assert_eq!(ours, theirs, "{rel} differs from upstream");
    }
}
