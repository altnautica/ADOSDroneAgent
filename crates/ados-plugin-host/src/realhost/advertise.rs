//! `offload.advertise`: a plugin that runs perception offload reports the link
//! it holds, and the host publishes it as the offload-link sidecar the
//! perception-tier decision reads.
//!
//! The plugin supplies the facts (paired, bearer acceptable, the node's
//! address, id and model); the host stamps the schema version and write time
//! and writes the sidecar atomically. The sidecar goes stale after
//! [`ados_protocol::offload_link::OFFLOAD_LINK_STALE_MS`], so a plugin keeps a
//! link alive by re-advertising well inside that window, and a plugin that
//! stops advertising stops counting as an offload path on its own.

use super::*;
use ados_protocol::offload_link::{write_offload_link_to, OffloadLink};

/// Longest `target`, `device_id` or `model_id` accepted.
const MAX_FIELD_LEN: usize = 256;

fn bool_arg(args: &Value, key: &str) -> Result<bool, HostError> {
    map_get(args, key)
        .and_then(Value::as_bool)
        .ok_or_else(|| HostError::Rpc(format!("{key} missing or not a bool")))
}

/// An optional string field: absent or nil is `None`; anything else must be a
/// printable string of at most [`MAX_FIELD_LEN`] bytes.
fn opt_string_arg(args: &Value, key: &str) -> Result<Option<String>, HostError> {
    match map_get(args, key) {
        None | Some(Value::Nil) => Ok(None),
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| HostError::Rpc(format!("{key} must be a string or nil")))?;
            if s.is_empty() || s.len() > MAX_FIELD_LEN || s.chars().any(char::is_control) {
                return Err(HostError::Rpc(format!(
                    "{key} must be 1-{MAX_FIELD_LEN} printable bytes"
                )));
            }
            Ok(Some(s.to_string()))
        }
    }
}

impl RealHost {
    /// Stamp and write the advertised link.
    pub(super) fn advertise_offload(&self, args: &Value) -> Result<HostResult, HostError> {
        let link = OffloadLink::stamped(
            bool_arg(args, "paired")?,
            bool_arg(args, "bearer_acceptable")?,
            opt_string_arg(args, "target")?,
            opt_string_arg(args, "device_id")?,
            opt_string_arg(args, "model_id")?,
            now_ms_wall(),
        );
        write_offload_link_to(&self.offload_link_path, &link)
            .map_err(|e| HostError::Rpc(format!("offload link write failed: {e}")))?;
        Ok(Value::Map(vec![(Value::from("ok"), Value::Boolean(true))]))
    }
}

/// Wall-clock epoch milliseconds, the clock the sidecar readers compare with.
fn now_ms_wall() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
