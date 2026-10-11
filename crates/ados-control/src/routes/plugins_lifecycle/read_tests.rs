//! The bundled catalog pins published GitHub release assets.

use std::collections::BTreeSet;

use super::bundled_catalog;

const RELEASE_BASE: &str = "https://github.com/altnautica/ADOSExtensions/releases/download/";

/// Every entry points at a `<ext>-v<version>` release of ADOSExtensions whose
/// asset is `<id>-<version>.signed.adosplug`, pinned by a sha256. This is the
/// shape `scripts/sync-plugin-catalog.py` writes; a hand edit that breaks it
/// (an old tag scheme, a mismatched version, a missing pin) fails here.
#[test]
fn bundled_catalog_entries_are_pinned_github_release_assets() {
    let catalog = bundled_catalog();
    assert!(
        catalog.get("error").is_none(),
        "catalog does not parse: {catalog}"
    );
    let plugins = catalog["plugins"].as_array().expect("plugins array");
    assert!(!plugins.is_empty());

    let mut ids = BTreeSet::new();
    for entry in plugins {
        let id = entry["id"].as_str().expect("id");
        assert!(ids.insert(id), "{id}: listed twice");
        let version = entry["version"].as_str().expect("version");

        let sha = entry["archive_sha256"].as_str().unwrap_or_default();
        assert!(
            sha.len() == 64
                && sha
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{id}: archive_sha256 {sha:?} is not 64 lowercase hex digits"
        );

        let url = entry["download_url"].as_str().unwrap_or_default();
        let rest = url
            .strip_prefix(RELEASE_BASE)
            .unwrap_or_else(|| panic!("{id}: {url} is not an ADOSExtensions release asset"));
        let (tag, file) = rest
            .split_once('/')
            .unwrap_or_else(|| panic!("{id}: {url} has no tag segment"));
        assert!(
            !file.contains('/'),
            "{id}: tag {tag:?} is not a single path segment"
        );
        assert!(
            tag.ends_with(&format!("-v{version}")),
            "{id}: tag {tag} does not name version {version}"
        );
        assert!(
            file.ends_with(&format!("-{version}.signed.adosplug")),
            "{id}: asset {file} does not end -{version}.signed.adosplug"
        );
        assert_eq!(
            file,
            format!("{id}-{version}.signed.adosplug"),
            "{id}: asset name"
        );
    }
}
