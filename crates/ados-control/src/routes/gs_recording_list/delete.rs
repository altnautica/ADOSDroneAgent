//! `DELETE /api/v1/ground-station/recording/<segment>`.

use super::*;

/// The longest segment file name this surface will accept. mediamtx's own
/// `<stamp>` names are around 30 characters; this is generous and still bounds
/// the value before it reaches the filesystem.
pub(super) const MAX_SEGMENT_NAME: usize = 128;

/// Why a `DELETE .../recording/<segment>` names nothing this surface may remove.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SegmentReject {
    /// The name is not one this surface accepts — a `400`, never a `404`: it is
    /// the request that is wrong, and answering `404` would tell an operator the
    /// file is missing when the name never could have named a file.
    Invalid,
    /// The name is fine but resolves to nothing removable.
    Absent,
}

/// True when `segment` names one entry directly inside the recordings directory:
/// non-empty, bounded, no path separator, no parent hop, not a bare `.` or `..`.
///
/// The FIRST of two gates. Path traversal is THE hazard on a DELETE that takes
/// its target from the URL — axum percent-decodes the path parameter, so `%2e%2e%2f`
/// arrives here as `../` — and a name check alone is not enough, because a
/// symlink planted inside the recordings directory pointing at
/// `/etc/ados/config.yaml` passes every string test there is.
pub(super) fn is_plain_segment_name(segment: &str) -> bool {
    if segment.is_empty() || segment.len() > MAX_SEGMENT_NAME {
        return false;
    }
    if segment.contains('/') || segment.contains('\\') || segment.contains('\0') {
        return false;
    }
    if segment.contains("..") {
        return false;
    }
    // Exactly one normal component. This is what refuses `.`, `..`, and any
    // root/prefix form the OS would resolve somewhere other than where it reads.
    let mut components = Path::new(segment).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

/// Resolve `segment` to the regular file it names inside `dir`, or say why not.
///
/// The SECOND gate: the canonical resolved path must still be inside the
/// canonical recordings directory. Canonicalising resolves symlinks, so this is
/// what refuses a link planted inside the directory that points out of it — the
/// case a name check cannot see.
///
/// A directory is refused rather than removed: a DELETE on this surface takes one
/// segment file and must never recurse.
pub(super) async fn resolve_segment(dir: &Path, segment: &str) -> Result<PathBuf, SegmentReject> {
    if !is_plain_segment_name(segment) {
        return Err(SegmentReject::Invalid);
    }
    let base = tokio::fs::canonicalize(dir)
        .await
        .map_err(|_| SegmentReject::Absent)?;
    let resolved = tokio::fs::canonicalize(base.join(segment))
        .await
        .map_err(|_| SegmentReject::Absent)?;
    if !resolved.starts_with(&base) {
        return Err(SegmentReject::Invalid);
    }
    match tokio::fs::metadata(&resolved).await {
        Ok(meta) if meta.is_file() => Ok(resolved),
        _ => Err(SegmentReject::Absent),
    }
}

/// `DELETE .../recording/<segment>` → `{filename, deleted}`.
///
/// `404` `E_PROFILE_MISMATCH` off a drone-profile node, `400`
/// `E_INVALID_SEGMENT` for a name that is not one plain entry inside the
/// recordings directory (a traversal attempt lands here), `404`
/// `E_SEGMENT_NOT_FOUND` when it names nothing removable.
pub async fn delete_recording_segment(
    State(state): State<AppState>,
    AxumPath(segment): AxumPath<String>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let resolved = match resolve_segment(&recordings_dir(), &segment).await {
        Ok(path) => path,
        Err(SegmentReject::Invalid) => {
            tracing::warn!(segment = %segment, "recording delete refused: not a plain segment name");
            return error_body(
                StatusCode::BAD_REQUEST,
                "E_INVALID_SEGMENT",
                "segment must name one file inside the recordings directory",
            );
        }
        Err(SegmentReject::Absent) => {
            return error_body(
                StatusCode::NOT_FOUND,
                "E_SEGMENT_NOT_FOUND",
                "no such recording segment",
            )
        }
    };
    match tokio::fs::remove_file(&resolved).await {
        Ok(()) => json_ok(json!({"filename": segment, "deleted": true})),
        Err(err) => {
            tracing::warn!(segment = %segment, error = %err, "recording delete failed");
            error_body(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_SEGMENT_DELETE_FAILED",
                "the recording segment could not be removed",
            )
        }
    }
}
