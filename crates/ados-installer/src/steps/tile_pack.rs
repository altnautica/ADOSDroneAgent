//! Field tile pack: place the `--tile-pack <url|path>` PMTiles archive in the
//! tiles directory the ground station serves to the GCS over the LAN
//! (`GET /api/v1/ground-station/tiles`).
//!
//! The pack keeps its own file name, which must be `<name>.pmtiles` with a
//! plain `<name>` (the name the GCS selects it by). A URL is fetched with the
//! installer's stall-tolerant downloader; a local path is copied. Either way the
//! bytes land in a temporary sibling first and are renamed into place, because
//! the agent memory-maps packs and a pack rewritten in place under it would
//! corrupt the mapping. Without the flag the step does nothing, and an existing
//! pack is never removed.
//!
//! Optional: a pack that cannot be placed degrades the install, it never
//! aborts it.

use std::path::{Path, PathBuf};

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// The tiles directory the agent serves, `ADOS_TILES_DIR` or
/// `/var/ados/tiles`; the same resolution the control surface uses.
pub fn tiles_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_TILES_DIR").unwrap_or_else(|_| "/var/ados/tiles".to_string()))
}

/// Whether `source` names a download rather than a local file.
fn is_url(source: &str) -> bool {
    source.starts_with("https://") || source.starts_with("http://")
}

/// The file name the pack is stored under: the last path segment of the URL
/// (query and fragment dropped) or of the local path. It must be
/// `<name>.pmtiles` where `<name>` is a plain name the agent will list.
pub fn pack_file_name(source: &str) -> Result<String, String> {
    let path_part = if is_url(source) {
        source.split(['?', '#']).next().unwrap_or(source)
    } else {
        source
    };
    let file = path_part
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_string();
    let Some(name) = file.strip_suffix(".pmtiles") else {
        return Err(format!(
            "--tile-pack must name a .pmtiles file, got {source:?}"
        ));
    };
    let plain = !name.is_empty()
        && !name.starts_with('.')
        && !name
            .bytes()
            .any(|b| matches!(b, b'%' | b':' | 0) || b.is_ascii_control());
    if !plain {
        return Err(format!(
            "--tile-pack file name {file:?} is not a plain <name>.pmtiles"
        ));
    }
    Ok(file)
}

/// Copy a local pack into `dest` through a temporary sibling and a rename.
fn copy_into_place(src: &Path, dest: &Path) -> Result<(), String> {
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = std::fs::copy(src, &tmp)
        .map_err(|e| format!("copy {} failed: {e}", src.display()))
        .and_then(|_| {
            std::fs::rename(&tmp, dest).map_err(|e| format!("rename into place failed: {e}"))
        });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The first bytes of every PMTiles v3 archive.
const PMTILES_MAGIC: &[u8; 8] = b"PMTiles\x03";

/// Whether the file at `path` starts with the PMTiles v3 magic.
fn is_pmtiles_v3(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && &head == PMTILES_MAGIC
}

/// Place `source` in `dir`, returning the stored path. The bytes land in a
/// `<dest>.staging` sibling first and are only renamed over `dest` once they
/// carry the PMTiles v3 magic, so an error page or a wrong file never replaces
/// a working pack.
pub fn place_pack(source: &str, dir: &Path) -> Result<PathBuf, String> {
    let file = pack_file_name(source)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let dest = dir.join(&file);
    let mut staging = dest.as_os_str().to_owned();
    staging.push(".staging");
    let staging = PathBuf::from(staging);
    if is_url(source) {
        crate::net::fetch(source, &staging).map_err(|e| format!("download failed: {e:#}"))?;
    } else {
        copy_into_place(Path::new(source), &staging)?;
    }
    if !is_pmtiles_v3(&staging) {
        let _ = std::fs::remove_file(&staging);
        return Err("not a PMTiles v3 archive".into());
    }
    if let Err(e) = std::fs::rename(&staging, &dest) {
        let _ = std::fs::remove_file(&staging);
        return Err(format!("rename into place failed: {e}"));
    }
    Ok(dest)
}

/// The field tile pack step (see the module doc).
pub struct TilePack;

impl Step for TilePack {
    fn id(&self) -> &str {
        "tile_pack"
    }
    fn requires(&self) -> &[&str] {
        // After deps, which installs the curl the download shells out to.
        &["deps"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: the flag is the request, so a re-run with it places
        // the pack again and a run without it does nothing.
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        let Some(source) = ctx.args.tile_pack.clone().filter(|s| !s.trim().is_empty()) else {
            return StepOutcome::Skipped;
        };
        if ctx.profile != "ground_station" {
            tracing::warn!(
                profile = %ctx.profile,
                "a tile pack is served only by a ground station; placing it anyway"
            );
        }
        ctx.progress
            .activity(self.id(), format!("placing tile pack {source}"));
        match place_pack(source.trim(), &tiles_dir()) {
            Ok(dest) => {
                tracing::info!(pack = %dest.display(), "tile pack placed");
                StepOutcome::Ok
            }
            Err(reason) => StepOutcome::Failed(format!("tile pack not placed: {reason}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::Checkpoint;

    #[test]
    fn the_file_name_comes_from_the_url_or_the_path() {
        assert_eq!(
            pack_file_name("https://packs.example.com/field.pmtiles?v=2#x").unwrap(),
            "field.pmtiles"
        );
        assert_eq!(
            pack_file_name("/srv/packs/site-a.pmtiles").unwrap(),
            "site-a.pmtiles"
        );
        assert_eq!(pack_file_name("site-b.pmtiles").unwrap(), "site-b.pmtiles");
    }

    #[test]
    fn a_name_the_agent_would_not_list_is_refused() {
        for bad in [
            "https://packs.example.com/field.mbtiles",
            "https://packs.example.com/",
            "/tmp/.pmtiles",
            "/tmp/.hidden.pmtiles",
            "https://packs.example.com/a%20b.pmtiles",
        ] {
            assert!(pack_file_name(bad).is_err(), "{bad}");
        }
    }

    /// A minimal file carrying the PMTiles v3 magic followed by `body`.
    fn pack_bytes(body: &[u8]) -> Vec<u8> {
        let mut bytes = PMTILES_MAGIC.to_vec();
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn a_local_pack_is_copied_into_place_and_replaced_atomically() {
        let src_dir = tempfile::tempdir().unwrap();
        let tiles = tempfile::tempdir().unwrap();
        let dir = tiles.path().join("tiles");
        let src = src_dir.path().join("field.pmtiles");
        std::fs::write(&src, pack_bytes(b"first")).unwrap();
        let dest = place_pack(src.to_str().unwrap(), &dir).unwrap();
        assert_eq!(dest, dir.join("field.pmtiles"));
        assert_eq!(std::fs::read(&dest).unwrap(), pack_bytes(b"first"));
        std::fs::write(&src, pack_bytes(b"second")).unwrap();
        place_pack(src.to_str().unwrap(), &dir).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), pack_bytes(b"second"));
        assert!(!dir.join("field.pmtiles.tmp").exists());
        assert!(!dir.join("field.pmtiles.staging").exists());
    }

    #[test]
    fn a_file_that_is_not_a_pmtiles_archive_is_refused_and_the_old_pack_kept() {
        let src_dir = tempfile::tempdir().unwrap();
        let tiles = tempfile::tempdir().unwrap();
        let dir = tiles.path();
        let dest = dir.join("field.pmtiles");
        std::fs::write(&dest, pack_bytes(b"working")).unwrap();
        let src = src_dir.path().join("field.pmtiles");
        std::fs::write(&src, b"<!doctype html><html>Not found</html>").unwrap();
        let err = place_pack(src.to_str().unwrap(), dir).unwrap_err();
        assert_eq!(err, "not a PMTiles v3 archive");
        assert_eq!(std::fs::read(&dest).unwrap(), pack_bytes(b"working"));
        assert!(!dir.join("field.pmtiles.staging").exists());
    }

    #[test]
    fn a_missing_local_pack_fails_and_leaves_nothing_behind() {
        let tiles = tempfile::tempdir().unwrap();
        let missing = tiles.path().join("absent.pmtiles");
        assert!(place_pack(missing.to_str().unwrap(), tiles.path()).is_err());
        assert_eq!(std::fs::read_dir(tiles.path()).unwrap().count(), 0);
    }

    #[test]
    fn no_flag_skips_the_step() {
        let mut ctx = Ctx::for_test(Checkpoint::new());
        assert_eq!(TilePack.run(&mut ctx), StepOutcome::Skipped);
        ctx.args.tile_pack = Some("  ".to_string());
        assert_eq!(TilePack.run(&mut ctx), StepOutcome::Skipped);
    }

    #[test]
    fn step_metadata_is_optional_after_deps() {
        assert_eq!(TilePack.id(), "tile_pack");
        assert_eq!(TilePack.requires(), &["deps"]);
        assert!(matches!(TilePack.kind(), StepKind::Optional));
        assert!(TilePack.checkpoint().is_none());
    }
}
