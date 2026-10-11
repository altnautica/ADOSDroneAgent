//! Part reading, the upload size budget and streaming a model part to disk.

use super::*;

/// The cap on any non-file part: the metadata JSON and any part outside the
/// contract, which is drained under the same cap.
pub(super) const PART_MAX_BYTES: usize = 64 * 1024;

/// The upload header carrying the model file's hex sha256. A digest-signed
/// upload's signature covers this digest instead of the streamed body.
pub(super) const CONTENT_SHA256_HEADER: &str = "x-content-sha256";

/// The 413 for a non-file part past [`PART_MAX_BYTES`].
pub(super) fn part_too_large() -> Response {
    detail(
        StatusCode::PAYLOAD_TOO_LARGE,
        format!(
            "a non-file upload part is larger than the {} KiB limit",
            PART_MAX_BYTES / 1024
        ),
    )
}

/// The 401 for a model file that does not hash to `X-Content-SHA256`.
pub(super) fn digest_mismatch() -> Response {
    let body = Json(json!({"error": "E_DIGEST_MISMATCH"}));
    (StatusCode::UNAUTHORIZED, body).into_response()
}

/// The lowercase hex digest from `X-Content-SHA256`, `None` when absent; a
/// present value that is not 64 hex digits is a 400.
// The Err is the finished refusal response, built once on a cold path; boxing
// it would only add an allocation.
#[allow(clippy::result_large_err)]
pub(super) fn expected_digest(headers: &HeaderMap) -> Result<Option<String>, Response> {
    let Some(raw) = headers.get(CONTENT_SHA256_HEADER) else {
        return Ok(None);
    };
    let value = raw.to_str().unwrap_or_default().trim().to_ascii_lowercase();
    if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(Some(value))
    } else {
        Err(detail(
            StatusCode::BAD_REQUEST,
            "X-Content-SHA256 must be 64 hex digits",
        ))
    }
}

/// Read the `metadata` part chunk by chunk under [`PART_MAX_BYTES`] and parse
/// it. Past the cap is a 413; a read fault or invalid JSON is a 400.
#[allow(clippy::result_large_err)]
pub(super) async fn read_metadata(mut field: Field<'_>) -> Result<UploadMeta, Response> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > PART_MAX_BYTES {
                    return Err(part_too_large());
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                return Err(detail(
                    StatusCode::BAD_REQUEST,
                    format!("metadata read failed: {e}"),
                ))
            }
        }
    }
    serde_json::from_slice::<UploadMeta>(&buf).map_err(|e| {
        detail(
            StatusCode::BAD_REQUEST,
            format!("metadata is not valid JSON: {e}"),
        )
    })
}

/// Read and discard a part, refusing with 413 once it passes `cap` bytes.
#[allow(clippy::result_large_err)]
pub(super) async fn drain_part_capped(mut field: Field<'_>, cap: usize) -> Result<(), Response> {
    let mut seen: usize = 0;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                seen += chunk.len();
                if seen > cap {
                    return Err(part_too_large());
                }
            }
            Ok(None) => return Ok(()),
            Err(e) => {
                return Err(detail(
                    StatusCode::BAD_REQUEST,
                    format!("malformed upload: {e}"),
                ))
            }
        }
    }
}

/// The largest model file the route accepts.
pub(super) const MAX_MODEL_BYTES: u64 = 512 * 1024 * 1024;

/// The free space an upload must leave on the models filesystem. The models dir
/// usually shares a partition with the config, pairing and log files, and a full
/// partition breaks their atomic writes.
pub(super) const MIN_FREE_AFTER_UPLOAD: u64 = 1024 * 1024 * 1024;

/// How many bytes one upload may write: the size cap, and, when the
/// filesystem's free space is known, never more than leaves
/// [`MIN_FREE_AFTER_UPLOAD`] behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct UploadBudget {
    pub(super) max_bytes: u64,
    /// Whether `max_bytes` is set by the free-space floor rather than the cap.
    pub(super) space_bound: bool,
}

/// An upload refused for its size or for the space it would leave.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SizeRefusal {
    /// Over [`MAX_MODEL_BYTES`]: 413.
    TooLarge,
    /// Storing it would drop free space below [`MIN_FREE_AFTER_UPLOAD`]: 507.
    NoSpace,
}

impl IntoResponse for SizeRefusal {
    fn into_response(self) -> Response {
        match self {
            Self::TooLarge => detail(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "model file is larger than the {} MiB limit",
                    MAX_MODEL_BYTES / (1024 * 1024)
                ),
            ),
            Self::NoSpace => detail(
                StatusCode::INSUFFICIENT_STORAGE,
                format!(
                    "not enough free space: storing this model would leave less than {} MiB \
                     free on the models filesystem",
                    MIN_FREE_AFTER_UPLOAD / (1024 * 1024)
                ),
            ),
        }
    }
}

/// The budget for an upload given the models filesystem's free bytes. Unknown
/// free space leaves only the size cap.
pub(super) fn upload_budget(available: Option<u64>) -> Result<UploadBudget, SizeRefusal> {
    let Some(available) = available else {
        return Ok(UploadBudget {
            max_bytes: MAX_MODEL_BYTES,
            space_bound: false,
        });
    };
    let room = available.saturating_sub(MIN_FREE_AFTER_UPLOAD);
    if room == 0 {
        return Err(SizeRefusal::NoSpace);
    }
    Ok(UploadBudget {
        max_bytes: room.min(MAX_MODEL_BYTES),
        space_bound: room < MAX_MODEL_BYTES,
    })
}

impl UploadBudget {
    /// Whether `size_bytes` written so far is still inside the budget.
    pub(super) fn admit(self, size_bytes: u64) -> Result<(), SizeRefusal> {
        if size_bytes <= self.max_bytes {
            Ok(())
        } else if self.space_bound {
            Err(SizeRefusal::NoSpace)
        } else {
            Err(SizeRefusal::TooLarge)
        }
    }
}

/// Free bytes on the filesystem holding `dir`: the mounted volume whose mount
/// point is the longest prefix of the path. `None` when no volume matches.
pub(super) fn available_bytes(dir: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    disks
        .list()
        .iter()
        .filter(|d| dir.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(sysinfo::Disk::available_space)
}

/// A model file streamed to a tmp path in the models dir, with its sha256 + size
/// accumulated during the stream. Dropping it removes the tmp file, so an upload
/// that fails validation or write leaves nothing behind.
pub(super) struct StagedFile {
    pub(super) tmp_path: PathBuf,
    pub(super) sha256: String,
    pub(super) size_bytes: u64,
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        // Best-effort cleanup: a successful finalize renames the tmp away first,
        // so this only fires on the error / rejected paths.
        let _ = std::fs::remove_file(&self.tmp_path);
    }
}

/// Why a file part did not stage.
pub(super) enum StreamFailure {
    /// Over the size cap or the free-space floor.
    Refused(SizeRefusal),
    /// A read or write fault: a 400-worthy message.
    Fault(String),
}

/// Stream a multipart file field to a uniquely-named tmp file in `dir`, hashing
/// the bytes as they land. The tmp name carries the process id + a random suffix
/// so concurrent uploads never collide; the file is renamed to its id-derived
/// name only on finalize. The stream stops, and the partial file is removed, as
/// soon as the bytes received leave `budget`. Returns the staged file (with its
/// digest + size) or why it did not stage.
pub(super) async fn stream_field_to_tmp(
    dir: &Path,
    mut field: axum::extract::multipart::Field<'_>,
    budget: UploadBudget,
) -> Result<StagedFile, StreamFailure> {
    use tokio::io::AsyncWriteExt;

    let tmp_path = dir.join(format!(
        ".upload-{}-{}.tmp",
        std::process::id(),
        random_token()
    ));
    let mut file = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(|e| StreamFailure::Fault(format!("staging file not writable: {e}")))?;

    let mut hasher = Sha256::new();
    let mut size_bytes: u64 = 0;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                size_bytes += chunk.len() as u64;
                if let Err(refusal) = budget.admit(size_bytes) {
                    drop(file);
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(StreamFailure::Refused(refusal));
                }
                hasher.update(&chunk);
                if let Err(e) = file.write_all(&chunk).await {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(StreamFailure::Fault(format!("staging write failed: {e}")));
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(StreamFailure::Fault(format!("file read failed: {e}")));
            }
        }
    }
    if let Err(e) = file.flush().await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(StreamFailure::Fault(format!("staging flush failed: {e}")));
    }
    drop(file);

    Ok(StagedFile {
        tmp_path,
        sha256: hex::encode(hasher.finalize()),
        size_bytes,
    })
}

/// A short random hex token for the staging-file name, so two concurrent uploads
/// into the same dir never write the same tmp path.
pub(super) fn random_token() -> String {
    let mut buf = [0u8; 8];
    // getrandom is already a crate dep (the pairing-key source); a failure here is
    // implausible, but fall back to a time-derived token rather than panicking.
    if getrandom::fill(&mut buf).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        return format!("{nanos:016x}");
    }
    hex::encode(buf)
}
