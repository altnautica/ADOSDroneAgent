//! The cloud transport for the plugin auto-update engine.
//!
//! [`ados_plugin_host::auto_update`] decides what a daily check does; this
//! module is the [`UpdateSource`] it runs over, because this service holds the
//! pairing key, the pinned TLS client, the download allowlist and the broker
//! credentials:
//!
//! * the registry row comes from `GET {convex}/v1/plugins/<id>` with the
//!   device's `X-ADOS-Key` (a 404 means the registry has no row);
//! * an archive is fetched through the same allowlisted, size-capped download
//!   the cloud install command uses, then checked against the row's SHA-256;
//! * an `update_available` notice is queued for [`run_notice_publisher`], which
//!   publishes it at QoS 1 on `ados/{device}/plugin/update_available` over a
//!   dedicated broker session (`ados-{device}-plugin-update`, so it never kicks
//!   the relay's own session).
//!
//! A notice that cannot be delivered is dropped with a warning; the next daily
//! check raises it again, since nothing about the install changed.

use std::sync::Arc;
use std::time::Duration;

use ados_plugin_host::auto_update::{latest_version_row, UpdateSource, VersionRow};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::config::CloudConfig;
use crate::mqtt::topic_plugin_update_available;
use crate::mqtt::transport::{MqttQos, MqttTransport, RumqttcTransport};
use crate::pairing::PairingState;
use ados_plugin_host::download::{
    fetch_capped, verify_sha256, HttpDownloadSource, DOWNLOAD_MAX_BYTES,
};

/// Bound on one registry query.
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(30);
/// Notices waiting for the broker. A cycle raises at most one per install.
const NOTICE_QUEUE: usize = 64;
/// How long a notice waits for the notice session to come up.
const CONNECT_WAIT: Duration = Duration::from_secs(30);
/// Bound on one notice publish.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);

/// The registry URL for one plugin, or `None` for an id that is not a plain
/// registry id (the id becomes a path segment, so nothing may escape it).
pub fn registry_url(convex_url: &str, plugin_id: &str) -> Option<String> {
    let plain = !plugin_id.is_empty()
        && plugin_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    plain.then(|| {
        format!(
            "{}/v1/plugins/{plugin_id}",
            convex_url.trim_end_matches('/')
        )
    })
}

/// The engine's transport over this service's credentials.
pub struct CloudUpdateSource {
    convex_url: String,
    registry: reqwest::blocking::Client,
    download: HttpDownloadSource,
    notices: mpsc::Sender<Value>,
}

impl CloudUpdateSource {
    /// A source for `convex_url`, plus the receiver [`run_notice_publisher`]
    /// drains.
    pub fn new(convex_url: String) -> (Self, mpsc::Receiver<Value>) {
        let (notices, rx) = mpsc::channel(NOTICE_QUEUE);
        let registry = reqwest::blocking::Client::builder()
            .use_preconfigured_tls(crate::tls::client_config())
            .timeout(REGISTRY_TIMEOUT)
            .build()
            .expect("reqwest blocking client builds with the rustls config");
        let source = CloudUpdateSource {
            convex_url,
            registry,
            download: HttpDownloadSource::new(),
            notices,
        };
        (source, rx)
    }
}

impl UpdateSource for CloudUpdateSource {
    fn ready(&self) -> bool {
        !self.convex_url.is_empty() && PairingState::load().api_key().is_some()
    }

    fn latest(&self, plugin_id: &str) -> Result<Option<VersionRow>, String> {
        let pairing = PairingState::load();
        let key = pairing.api_key().ok_or("not paired")?;
        let url = registry_url(&self.convex_url, plugin_id)
            .ok_or_else(|| format!("plugin id {plugin_id:?} is not a registry id"))?;
        let resp = self
            .registry
            .get(&url)
            .header("X-ADOS-Key", key)
            .send()
            .map_err(|e| format!("registry query failed: {e}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("registry query answered HTTP {}", resp.status()));
        }
        let payload: Value = resp
            .json()
            .map_err(|e| format!("registry payload unreadable: {e}"))?;
        latest_version_row(&payload)
    }

    fn download(&self, url: &str, sha256: &str) -> Result<Vec<u8>, String> {
        let body =
            fetch_capped(&self.download, url, DOWNLOAD_MAX_BYTES).map_err(|e| e.to_string())?;
        verify_sha256(&body, sha256).map_err(|e| e.to_string())?;
        Ok(body)
    }

    fn notify(&self, notice: &Value) {
        if let Err(e) = self.notices.try_send(notice.clone()) {
            tracing::warn!(error = %e, "plugin_update_notice_dropped");
        }
    }
}

/// Publish queued notices until `shutdown`. The notice session is opened on the
/// first notice and kept for the life of the process, re-opened only when the
/// pairing key changes.
pub async fn run_notice_publisher(
    config: Arc<CloudConfig>,
    mut notices: mpsc::Receiver<Value>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut session: Option<(String, Arc<RumqttcTransport>)> = None;
    loop {
        let notice = tokio::select! {
            n = notices.recv() => match n {
                Some(n) => n,
                None => return,
            },
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
                continue;
            }
        };
        let Some(key) = PairingState::load().api_key().map(str::to_string) else {
            tracing::warn!("plugin_update_notice_unpaired");
            continue;
        };
        if session.as_ref().is_none_or(|(k, _)| *k != key) {
            session = config
                .relay_transport(Some("plugin-update"), &key)
                .and_then(|cfg| match RumqttcTransport::connect(&cfg) {
                    Ok(t) => Some((key.clone(), t)),
                    Err(e) => {
                        tracing::warn!(error = %e, "plugin_update_notice_lane_not_built");
                        None
                    }
                });
        }
        let Some((_, transport)) = session.as_ref() else {
            tracing::warn!("plugin_update_notice_no_broker");
            continue;
        };
        publish_notice(transport, &config.agent.device_id, &notice).await;
    }
}

async fn publish_notice(transport: &RumqttcTransport, device_id: &str, notice: &Value) {
    let deadline = tokio::time::Instant::now() + CONNECT_WAIT;
    while !transport.connected() {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!("plugin_update_notice_broker_down");
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let body = serde_json::to_vec(notice).unwrap_or_default();
    let topic = topic_plugin_update_available(device_id);
    match tokio::time::timeout(
        PUBLISH_TIMEOUT,
        transport.publish(&topic, MqttQos::AtLeastOnce, body),
    )
    .await
    {
        Ok(Ok(())) => tracing::info!(topic = %topic, "plugin_update_notice_published"),
        Ok(Err(e)) => tracing::warn!(error = %e, "plugin_update_notice_publish_failed"),
        Err(_) => tracing::warn!("plugin_update_notice_publish_timeout"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registry_id_becomes_one_path_segment() {
        assert_eq!(
            registry_url("https://convex-site.example.com/", "com.example.thermal").as_deref(),
            Some("https://convex-site.example.com/v1/plugins/com.example.thermal")
        );
    }

    #[test]
    fn an_id_that_could_leave_its_segment_is_refused() {
        for id in ["", "../admin", "a/b", "a?b=1", "a#b", "a b", "a%2F"] {
            assert_eq!(registry_url("https://c.example.com", id), None, "{id}");
        }
    }
}
