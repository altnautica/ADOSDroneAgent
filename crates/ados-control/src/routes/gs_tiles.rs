//! Field tile packs: PMTiles archives a ground station serves to the GCS over
//! the LAN, so a map renders with no internet uplink.
//!
//! - **`GET /api/v1/ground-station/tiles`** lists the packs in the tiles
//!   directory (`ADOS_TILES_DIR`, default `/var/ados/tiles`): every
//!   `<name>.pmtiles` whose header parses, as `{name, bounds: [minLon, minLat,
//!   maxLon, maxLat], minzoom, maxzoom, tile_type, usable, size_bytes, etag}`,
//!   sorted by name. Only the 127-byte header of each file is read.
//! - **`GET /api/v1/ground-station/tiles/{name}/{z}/{x}/{y}.mvt`** serves one
//!   tile as stored in the pack, with `Content-Type:
//!   application/vnd.mapbox-vector-tile`, the pack's tile compression as
//!   `Content-Encoding`, `Cache-Control: public, max-age=86400` and an `ETag`
//!   of the pack etag plus the coordinate (an `If-None-Match` hit is a `304`).
//!   A tile the pack does not hold is a `204`; an unknown pack is
//!   `404 E_TILE_PACK_NOT_FOUND`; a pack that is not `usable` is
//!   `409 E_TILE_PACK_UNUSABLE`; a malformed coordinate is
//!   `400 E_INVALID_TILE`. The router cannot match a parameter with a static
//!   suffix, so the last segment is captured whole and its `.mvt` is checked
//!   here.
//!
//! `usable` is true for a vector (MVT) pack whose tiles are gzip-compressed or
//! uncompressed: those bytes go out as stored and every browser decodes them.
//! Raster packs and brotli/zstd tiles are listed but not served.
//!
//! Both routes gate on the ground-station profile and answer
//! `404 {"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}}` elsewhere. Auth
//! is the normal `X-ADOS-Key` edge; the tile route is exempt from the
//! per-caller request budget (see [`crate::auth::is_tile_read`]) because one
//! map pan fetches dozens of tiles.
//!
//! Packs are memory-mapped and the open readers are kept per file, reopened
//! when the file's size or mtime changes. Replace a pack by writing a new file
//! and renaming it over the old one, as the installer does: rewriting a mapped
//! file in place under a running agent invalidates the mapping.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::UNIX_EPOCH;

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use parking_lot::Mutex;
use pmtiles::{AsyncPmTilesReader, Compression, Header, MmapBackend, NoCache, TileCoord, TileType};
use serde::Serialize;
use serde_json::json;
use tokio::io::AsyncReadExt;

use crate::state::AppState;

/// The pack file extension; a pack's `name` is the file name without it.
const PACK_EXT: &str = ".pmtiles";
/// The tile URL suffix the GCS requests.
const TILE_EXT: &str = ".mvt";
/// The fixed size of a PMTiles v3 header.
const HEADER_LEN: usize = 127;
/// The spec bound on the header plus the root directory.
const MAX_INITIAL_BYTES: u64 = 16_384;
/// Tile responses are immutable for a given pack etag.
const CACHE_CONTROL: &str = "public, max-age=86400";
const MVT_CONTENT_TYPE: &str = "application/vnd.mapbox-vector-tile";

type Reader = AsyncPmTilesReader<MmapBackend, NoCache>;

/// The tiles directory, `ADOS_TILES_DIR` or `/var/ados/tiles`.
fn tiles_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_TILES_DIR").unwrap_or_else(|_| "/var/ados/tiles".to_string()))
}

/// True when the node resolves to the ground-station profile.
fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role_at(
        &cfg.agent.profile,
        &state.pairing_paths.profile_conf,
        &state.pairing_paths.mesh_role,
    );
    profile == "ground-station"
}

/// The profile-mismatch `404` every ground-station route answers off a ground
/// station.
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

/// The `{"detail": {"error": {"code", "message"}}}` error object.
fn error_body(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"detail": {"error": {"code": code, "message": message}}})),
    )
        .into_response()
}

fn pack_not_found() -> Response {
    error_body(
        StatusCode::NOT_FOUND,
        "E_TILE_PACK_NOT_FOUND",
        "no tile pack with that name",
    )
}

// ---------------------------------------------------------------------------
// Pack files.
// ---------------------------------------------------------------------------

/// A pack name is one plain file-name segment: no separators, no dot segments,
/// no hidden files, no escapes or control bytes.
fn valid_pack_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name
            .bytes()
            .any(|b| matches!(b, b'/' | b'\\' | b'%' | b':' | 0) || b.is_ascii_control())
}

/// The file a pack name refers to, or `None` for a name that is not one plain
/// segment.
fn pack_path(dir: &Path, name: &str) -> Option<PathBuf> {
    valid_pack_name(name).then(|| dir.join(format!("{name}{PACK_EXT}")))
}

/// What identifies one version of a pack file: its size and mtime. A change
/// to either reopens the reader and changes every tile etag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackStamp {
    size: u64,
    mtime_ms: u128,
}

impl PackStamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis());
        Self {
            size: meta.len(),
            mtime_ms,
        }
    }

    fn etag(self) -> String {
        format!("{:x}-{:x}", self.size, self.mtime_ms)
    }
}

/// Read and validate a pack's header. Besides the header parse, the root
/// directory must lie inside the first 16 KiB and inside the file, which the
/// reader assumes when it slices it out.
async fn read_header(path: &Path, size: u64) -> Result<Header, String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open failed: {e}"))?;
    let mut buf = [0u8; HEADER_LEN];
    file.read_exact(&mut buf)
        .await
        .map_err(|e| format!("header read failed: {e}"))?;
    let le_u64 = |at: usize| {
        let mut word = [0u8; 8];
        word.copy_from_slice(&buf[at..at + 8]);
        u64::from_le_bytes(word)
    };
    let (root_offset, root_length) = (le_u64(8), le_u64(16));
    let root_end = root_offset.checked_add(root_length);
    let limit = size.min(MAX_INITIAL_BYTES);
    if root_offset < HEADER_LEN as u64 || root_end.is_none_or(|end| end > limit) {
        return Err("root directory out of bounds".to_string());
    }
    Header::try_from_bytes(Bytes::copy_from_slice(&buf)).map_err(|e| e.to_string())
}

/// The wire name of a pack's tile type.
fn tile_type_name(t: TileType) -> &'static str {
    match t {
        TileType::Mvt => "mvt",
        TileType::Png => "png",
        TileType::Jpeg => "jpg",
        TileType::Webp => "webp",
        _ => "unknown",
    }
}

/// A pack the tile route serves: MVT tiles stored gzip-compressed or plain.
fn is_usable(h: &Header) -> bool {
    h.tile_type == TileType::Mvt
        && matches!(h.tile_compression, Compression::Gzip | Compression::None)
}

/// One entry of the pack listing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PackInfo {
    pub name: String,
    /// `[minLon, minLat, maxLon, maxLat]`.
    pub bounds: [f64; 4],
    pub minzoom: u8,
    pub maxzoom: u8,
    pub tile_type: &'static str,
    pub usable: bool,
    pub size_bytes: u64,
    pub etag: String,
}

/// The packs in `dir`, sorted by name. An absent directory is the empty list;
/// a file whose header does not parse is skipped with a warning.
async fn list_packs(dir: &Path) -> Vec<PackInfo> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Vec::new();
    };
    let mut packs = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str().and_then(|f| f.strip_suffix(PACK_EXT)) else {
            continue;
        };
        if !valid_pack_name(name) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = tokio::fs::metadata(&path).await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        match read_header(&path, meta.len()).await {
            Ok(h) => packs.push(PackInfo {
                name: name.to_string(),
                bounds: [
                    h.min_longitude,
                    h.min_latitude,
                    h.max_longitude,
                    h.max_latitude,
                ],
                minzoom: h.min_zoom,
                maxzoom: h.max_zoom,
                tile_type: tile_type_name(h.tile_type),
                usable: is_usable(&h),
                size_bytes: meta.len(),
                etag: PackStamp::of(&meta).etag(),
            }),
            Err(error) => {
                tracing::warn!(pack = %name, %error, "tile_pack_header_invalid");
            }
        }
    }
    // A pack deleted from the directory releases its memory map.
    READERS.lock().retain(|path, _| path.exists());
    packs.sort_by(|a, b| a.name.cmp(&b.name));
    packs
}

// ---------------------------------------------------------------------------
// Open readers.
// ---------------------------------------------------------------------------

struct OpenPack {
    stamp: PackStamp,
    reader: Arc<Reader>,
}

/// The open reader per pack file.
static READERS: LazyLock<Mutex<HashMap<PathBuf, OpenPack>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Drop a cached reader, so the next request reopens the file.
fn forget(path: &Path) {
    READERS.lock().remove(path);
}

enum OpenError {
    Unusable,
    Invalid(String),
}

/// The reader for `path` at `stamp`: the cached one when the file is
/// unchanged, otherwise a fresh one once its header validates.
async fn open_pack(path: &Path, stamp: PackStamp) -> Result<Arc<Reader>, OpenError> {
    if let Some(open) = READERS.lock().get(path) {
        if open.stamp == stamp {
            return Ok(Arc::clone(&open.reader));
        }
    }
    let header = read_header(path, stamp.size)
        .await
        .map_err(OpenError::Invalid)?;
    if !is_usable(&header) {
        forget(path);
        return Err(OpenError::Unusable);
    }
    let reader = Arc::new(
        Reader::new_with_path(path)
            .await
            .map_err(|e| OpenError::Invalid(e.to_string()))?,
    );
    READERS.lock().insert(
        path.to_path_buf(),
        OpenPack {
            stamp,
            reader: Arc::clone(&reader),
        },
    );
    Ok(reader)
}

// ---------------------------------------------------------------------------
// Tile serving.
// ---------------------------------------------------------------------------

/// Parse `{z}/{x}/{y}.mvt` into a valid tile coordinate.
fn parse_coord(z: &str, x: &str, tile: &str) -> Option<TileCoord> {
    let y = tile.strip_suffix(TILE_EXT)?;
    TileCoord::new(z.parse().ok()?, x.parse().ok()?, y.parse().ok()?).ok()
}

/// Whether an `If-None-Match` header value matches `etag`.
fn etag_matches(if_none_match: Option<&HeaderValue>, etag: &str) -> bool {
    let Some(value) = if_none_match.and_then(|v| v.to_str().ok()) else {
        return false;
    };
    value.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

/// A tile response with the shared caching headers.
fn tile_response(status: StatusCode, etag: &str) -> axum::http::response::Builder {
    Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, CACHE_CONTROL)
        .header(header::ETAG, etag)
}

/// Serve one tile of the pack `name` in `dir`.
async fn serve_tile(
    dir: &Path,
    name: &str,
    z: &str,
    x: &str,
    tile: &str,
    if_none_match: Option<&HeaderValue>,
) -> Response {
    let Some(path) = pack_path(dir, name) else {
        return pack_not_found();
    };
    let Some(coord) = parse_coord(z, x, tile) else {
        return error_body(
            StatusCode::BAD_REQUEST,
            "E_INVALID_TILE",
            "expected {z}/{x}/{y}.mvt inside the tile pyramid",
        );
    };
    let meta = match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.is_file() => meta,
        _ => {
            forget(&path);
            return pack_not_found();
        }
    };
    let stamp = PackStamp::of(&meta);
    let reader = match open_pack(&path, stamp).await {
        Ok(reader) => reader,
        Err(OpenError::Unusable) => {
            return error_body(
                StatusCode::CONFLICT,
                "E_TILE_PACK_UNUSABLE",
                "the pack does not hold gzip or uncompressed vector tiles",
            );
        }
        Err(OpenError::Invalid(error)) => {
            tracing::warn!(pack = %name, %error, "tile_pack_open_failed");
            return error_body(
                StatusCode::CONFLICT,
                "E_TILE_PACK_UNUSABLE",
                "the pack could not be read",
            );
        }
    };
    let etag = format!(
        "\"{}-{}-{}-{}\"",
        stamp.etag(),
        coord.z(),
        coord.x(),
        coord.y()
    );
    if etag_matches(if_none_match, &etag) {
        return tile_response(StatusCode::NOT_MODIFIED, &etag)
            .body(axum::body::Body::empty())
            .map_or_else(
                |_| StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                IntoResponse::into_response,
            );
    }
    let encoding = reader.get_header().tile_compression.content_encoding();
    let built = match reader.get_tile(coord).await {
        Ok(Some(bytes)) => {
            let mut builder =
                tile_response(StatusCode::OK, &etag).header(header::CONTENT_TYPE, MVT_CONTENT_TYPE);
            if let Some(encoding) = encoding {
                builder = builder.header(header::CONTENT_ENCODING, encoding);
            }
            builder.body(axum::body::Body::from(bytes))
        }
        Ok(None) => tile_response(StatusCode::NO_CONTENT, &etag).body(axum::body::Body::empty()),
        Err(error) => {
            forget(&path);
            tracing::warn!(pack = %name, %error, "tile_read_failed");
            return error_body(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_TILE_READ_FAILED",
                "the tile could not be read from the pack",
            );
        }
    };
    built.map_or_else(
        |_| StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        IntoResponse::into_response,
    )
}

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/tiles` → `{"packs": [...]}`.
pub async fn get_tile_packs(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    Json(json!({ "packs": list_packs(&tiles_dir()).await })).into_response()
}

/// `GET /api/v1/ground-station/tiles/{name}/{z}/{x}/{y}.mvt` → one tile.
pub async fn get_tile(
    State(state): State<AppState>,
    AxumPath((name, z, x, tile)): AxumPath<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    serve_tile(
        &tiles_dir(),
        &name,
        &z,
        &x,
        &tile,
        headers.get(header::IF_NONE_MATCH),
    )
    .await
}

#[cfg(test)]
mod tests;
