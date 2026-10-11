//! The broker session, the socket reply writer and the reason clamp.

use super::*;

pub(super) struct Session {
    pub(super) key: String,
    pub(super) transport: Arc<RumqttcTransport>,
    pub(super) opened: Instant,
}

pub(super) fn open_session(config: &CloudConfig, key: &str) -> Option<Session> {
    let cfg = config.relay_transport(Some(PLUGIN_PUBLISH_LANE), key)?;
    tracing::info!("cloud-publish broker lane connecting");
    match RumqttcTransport::connect(&cfg) {
        Ok(transport) => Some(Session {
            key: key.to_string(),
            transport,
            opened: Instant::now(),
        }),
        Err(e) => {
            tracing::warn!(error = %e, "cloud-publish broker lane not built");
            None
        }
    }
}

pub(super) async fn write_reply(
    stream: &mut UnixStream,
    reply: &CloudPublishReply,
) -> std::io::Result<()> {
    let frame = reply
        .encode()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    tokio::time::timeout(REPLY_WRITE_TIMEOUT, stream.write_all(&frame))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "reply write timed out"))?
}

pub(super) fn truncate(mut reason: String) -> String {
    if let Some((cut, _)) = reason.char_indices().nth(MAX_REASON_CHARS) {
        reason.truncate(cut);
    }
    reason
}
