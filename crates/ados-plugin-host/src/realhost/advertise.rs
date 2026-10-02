//! `offload.advertise`: a plugin that runs perception offload reports the link
//! it holds, and the host publishes it as the offload-link sidecar the
//! perception-tier decision reads.
//!
//! The plugin supplies the facts (paired, bearer acceptable, the node's
//! address, id and model). The host does not take `paired` on the plugin's
//! word: it is stamped true only when the advertised `target` accepts a TCP
//! connection from this node. The host records the advertising plugin as the
//! link's owner, and while that link is fresh another plugin's advertisement is
//! refused, so two plugins cannot overwrite each other's link. The sidecar goes
//! stale after [`ados_protocol::offload_link::OFFLOAD_LINK_STALE_MS`], so a
//! plugin keeps a link alive by re-advertising well inside that window, and a
//! plugin that stops advertising stops counting as an offload path (and stops
//! owning the link) on its own.

use std::time::Duration;

use super::*;
use ados_protocol::offload_link::{read_offload_link_from, write_offload_link_to, OffloadLink};

/// How long the reachability probe of an advertised target may take.
const REACH_TIMEOUT: Duration = Duration::from_secs(1);

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

/// Whether `target` (`host:port`) accepts a TCP connection within
/// [`REACH_TIMEOUT`].
async fn reachable(target: &str) -> bool {
    matches!(
        tokio::time::timeout(REACH_TIMEOUT, tokio::net::TcpStream::connect(target)).await,
        Ok(Ok(_))
    )
}

impl RealHost {
    /// Check ownership, verify the link, then stamp and write it.
    pub(super) async fn advertise_offload(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let paired = bool_arg(args, "paired")?;
        let bearer_acceptable = bool_arg(args, "bearer_acceptable")?;
        let target = opt_string_arg(args, "target")?;
        let device_id = opt_string_arg(args, "device_id")?;
        let model_id = opt_string_arg(args, "model_id")?;
        let owner = read_offload_link_from(&self.offload_link_path, now_ms_wall())
            .and_then(|current| current.owner);
        if owner.as_deref().is_some_and(|o| o != plugin_id) {
            return Err(HostError::Rpc(
                "the offload link is advertised by another plugin".to_string(),
            ));
        }
        let verified = match (&target, paired) {
            (Some(t), true) => reachable(t).await,
            _ => false,
        };
        let link = OffloadLink::stamped(
            verified,
            bearer_acceptable,
            target,
            device_id,
            model_id,
            now_ms_wall(),
        )
        .owned_by(plugin_id);
        write_offload_link_to(&self.offload_link_path, &link)
            .map_err(|e| HostError::Rpc(format!("offload link write failed: {e}")))?;
        Ok(Value::Map(vec![
            (Value::from("ok"), Value::Boolean(true)),
            (Value::from("paired"), Value::Boolean(verified)),
        ]))
    }
}

/// Wall-clock epoch milliseconds, the clock the sidecar readers compare with.
fn now_ms_wall() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
