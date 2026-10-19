use super::*;
use std::sync::Arc;

use pmtiles::PmTilesWriter;
use serde_json::Value;

use crate::auth::PairingState;
use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
use crate::state::PairingPaths;

/// One fixture tile: its `(z, x, y)` and its bytes.
type FixtureTile<'a> = ((u8, u32, u32), &'a [u8]);

/// Write a pack of `tile_type` holding `tiles` at `dir/<name>.pmtiles`.
fn write_pack(dir: &Path, name: &str, tile_type: TileType, tiles: &[FixtureTile<'_>]) {
    let file = std::fs::File::create(dir.join(format!("{name}.pmtiles"))).unwrap();
    let mut writer = PmTilesWriter::new(tile_type)
        .min_zoom(0)
        .max_zoom(2)
        .bounds(10.0, 45.0, 10.5, 45.5)
        .create(file)
        .unwrap();
    for ((z, x, y), data) in tiles {
        writer
            .add_tile(TileCoord::new(*z, *x, *y).unwrap(), data)
            .unwrap();
    }
    writer.finalize().unwrap();
}

/// A tiles directory with one vector pack (`field`), one raster pack
/// (`aerial`), one file that is not a pack, and one `.pmtiles` that is not an
/// archive.
fn fixture_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_pack(
        dir.path(),
        "field",
        TileType::Mvt,
        &[((0, 0, 0), b"vector-z0"), ((1, 0, 0), b"vector-z1")],
    );
    write_pack(dir.path(), "aerial", TileType::Png, &[((0, 0, 0), b"png")]);
    std::fs::write(dir.path().join("notes.txt"), "not a pack").unwrap();
    std::fs::write(dir.path().join("broken.pmtiles"), "too short").unwrap();
    dir
}

async fn body_bytes(resp: Response) -> Bytes {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
}

async fn body_json(resp: Response) -> Value {
    serde_json::from_slice(&body_bytes(resp).await).unwrap()
}

async fn tile(dir: &Path, name: &str, z: &str, x: &str, y: &str) -> Response {
    serve_tile(dir, name, z, x, y, None).await
}

/// An `AppState` whose config resolves to `profile`.
fn state_with_profile(dir: &Path, profile: &str) -> AppState {
    std::fs::write(
        dir.join("config.yaml"),
        format!("agent:\n  profile: {profile}\n"),
    )
    .unwrap();
    let pairing_paths = PairingPaths {
        config: dir.join("config.yaml"),
        pairing_json: dir.join("pairing.json"),
        wfb_key_dir: dir.join("wfb"),
        bind_state: dir.join("bind-state.json"),
        profile_conf: dir.join("profile.conf"),
        mesh_role: dir.join("mesh-role"),
        relay_secret: dir.join("relay-peer-secret"),
    };
    AppState::new(
        Arc::new(PairingState::with_path(dir.join("pairing.json"))),
        StateIpcClient::disconnected(),
        MavlinkIpcClient::new(dir.join("absent-mavlink.sock")),
        LogdQueryClient::new(dir.join("absent-logd.sock")),
        dir.join("board.json"),
        pairing_paths,
        Arc::new(crate::dashboard_pin::DashboardPin::with_path(
            dir.join("dashboard-pin.json"),
        )),
        Arc::new(crate::mcp::McpTokenStore::with_path(
            dir.join("mcp-token.json"),
        )),
    )
}

// ── listing ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn listing_reports_every_parseable_pack_sorted_by_name() {
    let dir = fixture_dir();
    let packs = list_packs(dir.path()).await;
    let names: Vec<&str> = packs.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["aerial", "field"]);

    let field = &packs[1];
    assert_eq!(field.tile_type, "mvt");
    assert!(field.usable);
    assert_eq!((field.minzoom, field.maxzoom), (0, 2));
    assert_eq!(field.bounds, [10.0, 45.0, 10.5, 45.5]);
    let size = std::fs::metadata(dir.path().join("field.pmtiles"))
        .unwrap()
        .len();
    assert_eq!(field.size_bytes, size);
    assert!(field.etag.starts_with(&format!("{size:x}-")));

    let aerial = &packs[0];
    assert_eq!(aerial.tile_type, "png");
    assert!(!aerial.usable, "raster packs are listed but not served");
}

#[tokio::test]
async fn listing_serialises_to_the_gcs_contract() {
    let dir = fixture_dir();
    let body = serde_json::to_value(list_packs(dir.path()).await).unwrap();
    let field = &body[1];
    let mut keys: Vec<&str> = field
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "bounds",
            "etag",
            "maxzoom",
            "minzoom",
            "name",
            "size_bytes",
            "tile_type",
            "usable"
        ]
    );
    assert_eq!(field["bounds"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn an_absent_directory_lists_no_packs() {
    let dir = tempfile::tempdir().unwrap();
    assert!(list_packs(&dir.path().join("absent")).await.is_empty());
}

#[tokio::test]
async fn a_header_pointing_past_the_file_is_rejected_before_the_reader_opens() {
    let dir = tempfile::tempdir().unwrap();
    // A valid v3 magic with a root directory offset beyond the file.
    let mut raw = vec![0u8; HEADER_LEN];
    raw[..7].copy_from_slice(b"PMTiles");
    raw[7] = 3;
    raw[8..16].copy_from_slice(&127u64.to_le_bytes());
    raw[16..24].copy_from_slice(&4096u64.to_le_bytes());
    let path = dir.path().join("cut.pmtiles");
    std::fs::write(&path, &raw).unwrap();
    assert!(read_header(&path, raw.len() as u64).await.is_err());
    assert!(list_packs(dir.path()).await.is_empty());
    let resp = tile(dir.path(), "cut", "0", "0", "0.mvt").await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

// ── tiles ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_held_tile_is_served_as_stored_with_its_encoding() {
    let dir = fixture_dir();
    let resp = tile(dir.path(), "field", "1", "0", "0.mvt").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let headers = resp.headers().clone();
    assert_eq!(headers[header::CONTENT_TYPE], MVT_CONTENT_TYPE);
    assert_eq!(headers[header::CONTENT_ENCODING], "gzip");
    assert_eq!(headers[header::CACHE_CONTROL], CACHE_CONTROL);
    let etag = headers[header::ETAG].to_str().unwrap().to_string();
    assert!(
        etag.starts_with('"') && etag.ends_with("-1-0-0\""),
        "{etag}"
    );
    let body = body_bytes(resp).await;
    assert_eq!(&body[..2], &[0x1f, 0x8b], "gzip magic");
}

#[tokio::test]
async fn a_tile_the_pack_does_not_hold_is_a_204() {
    let dir = fixture_dir();
    let resp = tile(dir.path(), "field", "2", "3", "3.mvt").await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(body_bytes(resp).await.is_empty());
}

#[tokio::test]
async fn a_matching_if_none_match_is_a_304() {
    let dir = fixture_dir();
    let first = tile(dir.path(), "field", "0", "0", "0.mvt").await;
    let etag = first.headers()[header::ETAG].clone();
    let resp = serve_tile(dir.path(), "field", "0", "0", "0.mvt", Some(&etag)).await;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(resp.headers()[header::ETAG], etag);
    let other = HeaderValue::from_static("\"something-else\"");
    let resp = serve_tile(dir.path(), "field", "0", "0", "0.mvt", Some(&other)).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn an_unknown_or_unsafe_pack_name_is_not_found() {
    let dir = fixture_dir();
    for name in ["missing", "..", ".hidden", "a\\b", "a%2fb", "notes"] {
        let resp = tile(dir.path(), name, "0", "0", "0.mvt").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{name}");
        assert_eq!(
            body_json(resp).await["detail"]["error"]["code"],
            "E_TILE_PACK_NOT_FOUND",
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_raster_pack_is_unusable_on_the_vector_route() {
    let dir = fixture_dir();
    let resp = tile(dir.path(), "aerial", "0", "0", "0.mvt").await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        "E_TILE_PACK_UNUSABLE"
    );
}

#[tokio::test]
async fn a_malformed_coordinate_is_a_400() {
    let dir = fixture_dir();
    for (z, x, y) in [
        ("0", "0", "0"),
        ("0", "0", "0.png"),
        ("a", "0", "0.mvt"),
        ("1", "2", "0.mvt"),
        ("-1", "0", "0.mvt"),
    ] {
        let resp = tile(dir.path(), "field", z, x, y).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{z}/{x}/{y}");
        assert_eq!(
            body_json(resp).await["detail"]["error"]["code"],
            "E_INVALID_TILE"
        );
    }
}

#[tokio::test]
async fn a_replaced_pack_is_reopened() {
    let dir = fixture_dir();
    let before = tile(dir.path(), "field", "0", "0", "0.mvt").await;
    assert_eq!(before.status(), StatusCode::OK);
    let old_etag = before.headers()[header::ETAG].clone();
    // Replace by rename, the supported way: a new file, then over the old name.
    let staged = tempfile::tempdir().unwrap();
    write_pack(
        staged.path(),
        "field",
        TileType::Mvt,
        &[((0, 0, 0), b"replacement-tile-with-a-different-length")],
    );
    let target = dir.path().join("field.pmtiles");
    // Windows refuses to rename over a mapped file, so the dev host drops the
    // cached reader first; on Linux the open mapping keeps the old inode and
    // the changed stamp alone forces the reopen.
    #[cfg(windows)]
    forget(&target);
    std::fs::rename(staged.path().join("field.pmtiles"), &target).unwrap();
    let after = tile(dir.path(), "field", "0", "0", "0.mvt").await;
    assert_eq!(after.status(), StatusCode::OK);
    assert_ne!(after.headers()[header::ETAG], old_etag);
    // The z1 tile the old pack held is gone from the new one.
    let gone = tile(dir.path(), "field", "1", "0", "0.mvt").await;
    assert_eq!(gone.status(), StatusCode::NO_CONTENT);
}

#[test]
fn if_none_match_accepts_lists_weak_tags_and_wildcards() {
    let etag = "\"a-1-2-3\"";
    let check = |v: &'static str| etag_matches(Some(&HeaderValue::from_static(v)), etag);
    assert!(check("\"a-1-2-3\""));
    assert!(check("W/\"a-1-2-3\""));
    assert!(check("\"x\", \"a-1-2-3\""));
    assert!(check("*"));
    assert!(!check("\"a-1-2-4\""));
    assert!(!etag_matches(None, etag));
}

// ── profile gate ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_drone_answers_profile_mismatch_on_both_routes() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with_profile(dir.path(), "drone");
    let want = json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}});

    let resp = get_tile_packs(State(state.clone())).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await, want);

    let resp = get_tile(
        State(state),
        AxumPath((
            "field".to_string(),
            "0".to_string(),
            "0".to_string(),
            "0.mvt".to_string(),
        )),
        HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await, want);
}

#[tokio::test]
async fn a_ground_station_passes_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with_profile(dir.path(), "ground_station");
    let resp = get_tile_packs(State(state)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_json(resp).await["packs"].is_array());
}
