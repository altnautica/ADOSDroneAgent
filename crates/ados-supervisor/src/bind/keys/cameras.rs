//! Video camera-leg persistence: merging operator and driver-owned legs into
//! the top-level `cameras` list in `config.yaml`.

use super::*;

/// The owner tag of an existing config leg, defaulting to `operator` for a leg
/// written before the field existed (legacy legs are operator-managed).
pub(super) fn leg_owner(leg: &serde_json::Value) -> String {
    leg.get("owner")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(OPERATOR_OWNER)
        .to_string()
}

/// Stamp every incoming leg with the writer's `owner`, so the merge can attribute
/// each declared leg to the operator or to the plugin that declared it. Only
/// object legs are stamped (a malformed non-object is passed through untouched;
/// the caller has already validated the shape).
pub(super) fn stamp_owner(legs: &[serde_json::Value], owner: &str) -> Vec<serde_json::Value> {
    legs.iter()
        .map(|leg| {
            let mut leg = leg.clone();
            if let Some(obj) = leg.as_object_mut() {
                obj.insert(
                    "owner".to_string(),
                    serde_json::Value::String(owner.to_string()),
                );
            }
            leg
        })
        .collect()
}

/// The owner the operator's Cameras surface writes under. A leg with no owner tag
/// (written before the field existed) reads as this owner.
pub const OPERATOR_OWNER: &str = "operator";

/// The effective primary of a leg list: the first leg with role `primary`, else
/// the first leg (matching `VideoConfig::resolve_legs`).
pub(super) fn effective_primary(legs: &[serde_json::Value]) -> Option<&serde_json::Value> {
    legs.iter()
        .find(|c| c.get("role").and_then(serde_json::Value::as_str) == Some("primary"))
        .or_else(|| legs.first())
}

/// Merge an owner's incoming leg list into the existing list.
///
/// An operator write preserves a plugin's declared legs (a smart pod's streams)
/// and a plugin write preserves the operator's and other plugins' legs. The
/// writer's own existing legs are replaced by the incoming block, which is
/// spliced where the writer's first existing leg was, or appended when the
/// writer had none; every other leg keeps its position.
///
/// The write is refused (`Err` with an `owner_conflict` reason) when:
/// - an incoming id matches a leg another owner holds. A leg with no owner tag
///   belongs to the operator, so only the operator replaces it;
/// - a plugin declares a `primary` leg while the operator's leg is the current
///   primary. The primary leg feeds the main stream (radio video, cloud relay and
///   the vision tap), so a plugin may only take it on a node whose operator
///   declared none.
pub(super) fn merge_camera_legs(
    existing: &[serde_json::Value],
    incoming_stamped: &[serde_json::Value],
    owner: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let incoming_ids: std::collections::HashSet<&str> = incoming_stamped
        .iter()
        .filter_map(|l| l.get("id").and_then(serde_json::Value::as_str))
        .collect();
    for leg in existing {
        let holder = leg_owner(leg);
        if holder == owner {
            continue;
        }
        if let Some(id) = leg
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| incoming_ids.contains(id))
        {
            return Err(format!(
                "owner_conflict: camera id {id:?} belongs to {holder}"
            ));
        }
    }
    if owner != OPERATOR_OWNER {
        let declares_primary = incoming_stamped
            .iter()
            .any(|l| l.get("role").and_then(serde_json::Value::as_str) == Some("primary"));
        let operator_primary = effective_primary(existing)
            .map(|leg| leg_owner(leg) == OPERATOR_OWNER)
            .unwrap_or(false);
        if declares_primary && operator_primary {
            return Err(
                "owner_conflict: the operator's camera is the primary; a plugin cannot replace it"
                    .to_string(),
            );
        }
    }
    let incoming = overlay_operator_legs(existing, incoming_stamped, owner);
    for leg in &incoming {
        if let Err(e) = ados_config::cameras::check_entry_json(leg) {
            let id = leg
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            return Err(format!("camera {id:?} is invalid: {e}"));
        }
    }
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut spliced = false;
    for leg in existing {
        if leg_owner(leg) == owner {
            if !spliced {
                out.extend(incoming.iter().cloned());
                spliced = true;
            }
            // Drop the replaced leg.
        } else {
            out.push(leg.clone());
        }
    }
    if !spliced {
        out.extend(incoming);
    }
    Ok(out)
}

/// An operator write is a per-entry patch: each incoming leg is laid over the
/// operator's existing leg of the same id, so a key the client did not send (or
/// does not know) survives, and an explicit `null` removes the key. A plugin
/// write replaces its legs whole. Null members are never persisted.
fn overlay_operator_legs(
    existing: &[serde_json::Value],
    incoming_stamped: &[serde_json::Value],
    owner: &str,
) -> Vec<serde_json::Value> {
    incoming_stamped
        .iter()
        .map(|leg| {
            let Some(patch) = leg.as_object() else {
                return leg.clone();
            };
            let id = patch.get("id").and_then(serde_json::Value::as_str);
            let base = (owner == OPERATOR_OWNER)
                .then(|| {
                    existing.iter().find(|l| {
                        leg_owner(l) == owner
                            && l.get("id").and_then(serde_json::Value::as_str) == id
                    })
                })
                .flatten()
                .and_then(serde_json::Value::as_object);
            let mut merged = base.cloned().unwrap_or_default();
            for (k, v) in patch {
                if v.is_null() {
                    merged.remove(k);
                } else {
                    merged.insert(k.clone(), v.clone());
                }
            }
            serde_json::Value::Object(merged)
        })
        .collect()
}

/// Merge the writer's `cameras` (attributed to `owner`) into the existing
/// top-level `cameras` list on the config mapping, then mirror the resulting
/// primary leg's source into `video.camera.source`. A non-array `cameras` leaves the
/// config untouched — the caller validates the list shape before this is reached,
/// so this is defence-in-depth. A write that would take over another owner's leg
/// (see [`merge_camera_legs`]) is refused with the config untouched.
pub fn apply_video_cameras(
    root: &mut Mapping,
    cameras: &serde_json::Value,
    owner: &str,
) -> Result<(), String> {
    let Some(incoming) = cameras.as_array() else {
        return Ok(());
    };

    // The existing declared legs, read back as JSON so the merge works in one data
    // model (an absent / non-sequence `cameras` reads as empty).
    let existing: Vec<serde_json::Value> = root
        .get("cameras")
        .and_then(Value::as_sequence)
        .map(|seq| {
            seq.iter()
                .filter_map(|v| serde_json::to_value(v).ok())
                .collect()
        })
        .unwrap_or_default();

    let stamped = stamp_owner(incoming, owner);
    let merged = merge_camera_legs(&existing, &stamped, owner)?;

    // Mirror the PRIMARY merged leg's source into `video.camera.source`, so the
    // inline video pipeline (which reads `video.camera`) serves the primary leg. A
    // pod-only drone has no local camera — its primary leg is a network RTSP the
    // existing IP-camera path pulls into `/main`. Without this, the pipeline runs
    // local V4L2/CSI discovery, finds nothing, and never starts (zero video).
    if let Some(src) = effective_primary(&merged)
        .and_then(|c| c.get("source"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let camera = ensure_map(ensure_map(root, "video"), "camera");
        set_str(camera, "source", src);
    }

    // Transcode the merged JSON list into the YAML data model and write it back.
    let Ok(value) = serde_norway::to_value(&merged) else {
        return Ok(());
    };
    root.insert(Value::String("cameras".to_string()), value);
    Ok(())
}

/// The outcome of [`persist_video_cameras`].
#[derive(Debug, PartialEq, Eq)]
pub enum CamerasPersist {
    /// The merged list was written.
    Written,
    /// The write would take over another owner's leg; nothing was written.
    Refused(String),
    /// The config could not be read or written (or the caller is not root).
    Failed,
}

/// Load → merge `cameras` by `owner` → atomically rewrite the config,
/// serialised by the same flock and euid-0 gate as [`persist_pair_state`] (the
/// file is 0600 root). A non-root caller on Linux gets `Failed` (no write).
/// The video pipeline reads the new source list on its next start, so the caller
/// restarts `ados-video` after `Written`.
pub fn persist_video_cameras(
    config_path: &Path,
    lock_path: &Path,
    cameras: &serde_json::Value,
    owner: &str,
) -> CamerasPersist {
    #[cfg(target_os = "linux")]
    {
        if !nix::unistd::geteuid().is_root() {
            tracing::error!(
                path = %config_path.display(),
                euid = nix::unistd::geteuid().as_raw(),
                "config_write_requires_root"
            );
            return CamerasPersist::Failed;
        }
    }

    #[cfg(target_os = "linux")]
    let _lock = acquire_config_lock(lock_path);
    #[cfg(not(target_os = "linux"))]
    let _ = lock_path;

    let mut root = match try_load_config_mapping(config_path) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, path = %config_path.display(), "config_write_aborted_unparseable");
            return CamerasPersist::Failed;
        }
    };
    if let Err(reason) = apply_video_cameras(&mut root, cameras, owner) {
        tracing::warn!(owner, reason = %reason, "camera_legs_write_refused");
        return CamerasPersist::Refused(reason);
    }

    let body = match serde_norway::to_string(&Value::Mapping(root)) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, path = %config_path.display(), "config_write_failed");
            return CamerasPersist::Failed;
        }
    };
    match write_atomic(config_path, body.as_bytes(), 0o600) {
        Ok(()) => CamerasPersist::Written,
        Err(e) => {
            tracing::error!(error = %e, path = %config_path.display(), "config_write_failed");
            CamerasPersist::Failed
        }
    }
}

#[cfg(test)]
mod tests;
