//! Command-socket forwards to the GPIO, radio aux-stream and video services, and
//! the shared aux-subscribe reader.

use super::*;

// ---------------------------------------------------------------------
// Command-socket forward (GPIO, radio aux, video)
// ---------------------------------------------------------------------

/// Cap on the reply read so a misbehaving service can't grow the buffer.
pub(super) const COMMAND_REPLY_CAP: u64 = 64 * 1024;

/// Budget for one GPIO or radio aux command-socket exchange. Both services
/// answer at once (a beep schedules and returns; an aux open or close is a local
/// state change), so a reply later than this is a wedged service.
pub(super) const COMMAND_FORWARD_TIMEOUT: Duration = Duration::from_secs(2);

/// Budget for one `video.source.set` exchange. The supervisor persists the
/// camera list and then restarts the video service, a systemctl action it bounds
/// at 30 s, before it replies. A shorter budget reports a change that is still
/// being applied as unavailable, and a driver that retries on that restarts the
/// pipeline again.
pub(super) const VIDEO_FORWARD_TIMEOUT: Duration = Duration::from_secs(40);

/// Send one newline-JSON request to a unix command socket and read the one-line
/// JSON reply, the whole exchange bounded by `budget`. Async, so a slow or
/// wedged service parks only the calling request, never a runtime worker. The
/// forward mirrors the REST layer's radio/wifi command-socket clients.
#[cfg(unix)]
pub(super) async fn command_socket_roundtrip(
    sock_path: &std::path::Path,
    request: &serde_json::Value,
    budget: Duration,
) -> std::io::Result<serde_json::Value> {
    let exchange = async {
        let mut stream = tokio::net::UnixStream::connect(sock_path).await?;
        let mut body = serde_json::to_vec(request)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        body.push(b'\n');
        stream.write_all(&body).await?;
        let mut line = Vec::new();
        tokio::io::BufReader::new(stream)
            .take(COMMAND_REPLY_CAP)
            .read_until(b'\n', &mut line)
            .await?;
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        serde_json::from_slice(&line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    };
    tokio::time::timeout(budget, exchange).await.map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("no reply within {} ms", budget.as_millis()),
        )
    })?
}

/// Non-unix dev hosts have no unix-socket command plane: report the service as
/// unreachable so the forward degrades to `not_available`.
#[cfg(not(unix))]
pub(super) async fn command_socket_roundtrip(
    _sock_path: &std::path::Path,
    _request: &serde_json::Value,
    _budget: Duration,
) -> std::io::Result<serde_json::Value> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no unix socket on this platform",
    ))
}

/// One command-socket forward: the exchange bounded by `budget`, with a service
/// that did not answer mapped to the `not_available` reply. The `reason` tells a
/// wedged service (`"<service> service did not answer within N ms"`) apart from
/// one that is not there (`"<service> service unavailable"`), so a long apply
/// that overran its budget never reads as an absent service.
async fn forward_to(
    service: &str,
    sock_path: &std::path::Path,
    request: &serde_json::Value,
    budget: Duration,
    method: &str,
) -> Result<serde_json::Value, HostResult> {
    command_socket_roundtrip(sock_path, request, budget)
        .await
        .map_err(|e| {
            tracing::debug!(method, service, error = %e, "command forward failed");
            let reason = if e.kind() == std::io::ErrorKind::TimedOut {
                format!(
                    "{service} service did not answer within {} ms",
                    budget.as_millis()
                )
            } else {
                format!("{service} service unavailable")
            };
            service_unavailable(method, &reason)
        })
}

/// The graceful-degrade reply for a forward whose service did not answer: the
/// socket is absent, refused the connection, or missed its budget.
pub(super) fn service_unavailable(method: &str, reason: &str) -> HostResult {
    Value::Map(vec![
        (Value::from("error"), Value::from("not_available")),
        (Value::from("method"), Value::from(method)),
        (Value::from("reason"), Value::from(reason)),
    ])
}

/// Canonical GPIO-output command socket the host forwards `gpio.*` methods to.
/// Kept in sync with `ados_gpio::GPIO_CMD_SOCK` by the cross-crate wire string,
/// not a build dependency (the gpio crate is not on the host's dependency path).
pub(super) const GPIO_CMD_SOCK: &str = "/run/ados/gpio-cmd.sock";

/// Canonical radio auxiliary-stream command socket the host forwards
/// `radio.aux_stream.*` methods to. Kept in sync with
/// `ados_radio::paths::RADIO_AUX_SOCK` by the cross-crate wire string, not a build
/// dependency (the radio crate is not on the host's dependency path).
pub(super) const RADIO_AUX_CMD_SOCK: &str = "/run/ados/radio-aux.sock";

/// Canonical supervisor video command socket the host forwards `video.source.set`
/// to. Kept in sync with the supervisor's `VIDEO_CMD_SOCK` by the cross-crate
/// wire string, not a build dependency (the supervisor crate is not on the host's
/// dependency path).
pub(super) const VIDEO_CMD_SOCK: &str = "/run/ados/video-cmd.sock";

/// Broadcast depth of the host's shared aux-subscribe reader. App frames are
/// lossy-tolerant by design (the ARQ lives at the application layer), so a
/// subscriber this far behind drops oldest first rather than growing without
/// bound. Matches the button bus depth.
pub(super) const AUX_BROADCAST_DEPTH: usize = 256;

/// One aux-subscribe connection's lifetime: subscribe, then re-broadcast each
/// decoded application datagram until EOF or error. Called by the reader task,
/// which reconnects on the shared fixed interval. The first line is the service's
/// `{"ok":true}` ack (or a `{"ok":false,...}` refuse); every later line is a
/// `{"channel":N,"payload":[...]}` datagram.
pub(super) async fn aux_subscribe_pump(
    path: &std::path::Path,
    tx: &tokio::sync::broadcast::Sender<(u8, Vec<u8>)>,
) -> std::io::Result<()> {
    let mut stream = tokio::net::UnixStream::connect(path).await?;
    stream.write_all(b"{\"op\":\"subscribe\"}\n").await?;
    stream.flush().await?;

    let mut lines = tokio::io::BufReader::new(stream).lines();
    // The single ack / refuse line. A refuse (e.g. E_AUX_DISABLED) is a quiet,
    // resting state: the stream closes right after and the reader retries, so a
    // disabled deployment arms but yields nothing (mirrors the button/vision
    // pumps when their sources are absent).
    if let Some(ack) = lines.next_line().await? {
        if !ack.contains("\"ok\":true") {
            tracing::debug!(error = %ack, "aux subscribe refused");
        }
    }
    while let Some(line) = lines.next_line().await? {
        if let Some((channel, payload)) = decode_aux_line(&line) {
            // `send` fails only when nobody is subscribed, which is the normal
            // case on a node with no aux-using plugin. Not an error.
            let _ = tx.send((channel, payload));
        }
    }
    Ok(())
}

/// Decode one aux-subscribe stream line into its application (`channel`,
/// `payload`). Returns `None` for a line that is not a well-formed datagram. The
/// channel is passed through as the AppStream/AppCommand byte; the payload is
/// the application bytes the service forwarded (the host never decodes it).
pub(super) fn decode_aux_line(line: &str) -> Option<(u8, Vec<u8>)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let channel = v.get("channel")?.as_u64()? as u8;
    let payload = v
        .get("payload")?
        .as_array()?
        .iter()
        .filter_map(|n| n.as_u64().and_then(|n| u8::try_from(n).ok()))
        .collect::<Vec<u8>>();
    Some((channel, payload))
}

impl RealHost {
    /// Forward a built `gpio.*` request to the GPIO-output service's command
    /// socket and return its reply as the plugin's response `args`. The service
    /// answers a `set` / `beep` request immediately (a beep schedules and
    /// returns), so the exchange is bounded by [`COMMAND_FORWARD_TIMEOUT`].
    ///
    /// A missing service / connection / IO error / missed budget degrades to the
    /// `not_available` shape rather than erroring, matching the `mavlink.send`
    /// and `process.spawn` not-available paths — the GPIO service may simply not
    /// be up on this board.
    pub(super) async fn forward_gpio(
        &self,
        request: serde_json::Value,
        method: &str,
    ) -> HostResult {
        match forward_to(
            "gpio",
            &self.gpio_cmd_path,
            &request,
            COMMAND_FORWARD_TIMEOUT,
            method,
        )
        .await
        {
            Ok(reply) => json_to_mpv(&reply),
            Err(unavailable) => unavailable,
        }
    }

    /// Forward a built `video.source.set` request to the supervisor's video
    /// command socket and return its reply. The supervisor persists the source
    /// list and restarts the video service before it answers, so the exchange
    /// gets [`VIDEO_FORWARD_TIMEOUT`], longer than that restart.
    ///
    /// A missing service / connection / IO error / missed budget degrades to the
    /// `not_available` shape rather than erroring, matching the GPIO / radio
    /// not-available paths — the supervisor may not be up (e.g. an early-boot
    /// window or a profile with no video pipeline).
    pub(super) async fn forward_video(
        &self,
        request: serde_json::Value,
        method: &str,
    ) -> HostResult {
        match forward_to(
            "video",
            &self.video_cmd_path,
            &request,
            VIDEO_FORWARD_TIMEOUT,
            method,
        )
        .await
        {
            Ok(reply) => json_to_mpv(&reply),
            Err(unavailable) => unavailable,
        }
    }

    /// Forward a built `radio.aux_stream.*` request to the radio service's
    /// auxiliary command socket and return its reply. The service answers an
    /// open/close at once, so the exchange is bounded by
    /// [`COMMAND_FORWARD_TIMEOUT`]. Returns `(reply, ok)` so the caller can update
    /// the owner bookkeeping only when the service confirmed the apply.
    ///
    /// A missing service / connection / IO error / missed budget degrades to the
    /// `not_available` shape rather than erroring, matching the GPIO / mavlink
    /// not-available paths — the aux lane's service may not be up (e.g. a drone
    /// with no adapter).
    pub(super) async fn forward_radio_aux(
        &self,
        request: serde_json::Value,
        method: &str,
    ) -> (HostResult, bool) {
        match forward_to(
            "radio",
            &self.radio_aux_cmd_path,
            &request,
            COMMAND_FORWARD_TIMEOUT,
            method,
        )
        .await
        {
            Ok(reply) => {
                let ok = reply.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                (json_to_mpv(&reply), ok)
            }
            Err(unavailable) => (unavailable, false),
        }
    }

    /// Require that `plugin_id` owns the auxiliary stream, so a plugin can
    /// neither tear down nor transmit on a stream another plugin (or an agent
    /// service) holds. Refused before anything reaches the radio service.
    pub(super) fn require_aux_owner(&self, plugin_id: &str) -> Result<(), HostError> {
        let owner = self
            .aux_stream_owner
            .lock()
            .expect("aux stream owner mutex poisoned");
        if owner.as_deref() == Some(plugin_id) {
            Ok(())
        } else {
            Err(HostError::Rpc(
                "radio aux stream is not open by this plugin".to_string(),
            ))
        }
    }
}
