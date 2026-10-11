//! The `hello` handshake and peer credential check.

use super::*;

/// The read and write halves of an authenticated connection, with the token
/// its `hello` verified.
pub(super) type Handshake = (
    CapabilityToken,
    tokio::net::unix::OwnedReadHalf,
    tokio::net::unix::OwnedWriteHalf,
);

/// Whether a peer may use a plugin's socket: root, this daemon's own user, or
/// the plugin's own account. The token alone is not enough, because every
/// plugin user shares the socket group. A plugin without a system account
/// (an unsandboxed host such as macOS, or a dev run) skips the check; so
/// does a peer whose credentials cannot be read.
pub(super) fn peer_uid_permitted(peer: Option<u32>, plugin_uid: Option<u32>, own_uid: u32) -> bool {
    if cfg!(target_os = "macos") {
        return true;
    }
    let (Some(peer), Some(plugin_uid)) = (peer, plugin_uid) else {
        return true;
    };
    peer == 0 || peer == own_uid || peer == plugin_uid
}

impl<H: HostServices> Connection<H> {
    /// Run the `hello` handshake: read one bounded frame, verify the token
    /// against this socket's plugin, answer `ready`. `Ok(None)` when the peer
    /// closed or was refused (the refusal was already sent).
    pub(super) async fn handshake(
        &self,
        stream: UnixStream,
    ) -> Result<Option<Handshake>, ServerError> {
        let peer_uid = stream.peer_cred().ok().map(|c| c.uid());
        let (mut read_half, mut write_half) = stream.into_split();
        if !peer_uid_permitted(
            peer_uid,
            crate::plugin_account::plugin_uid(&self.plugin_id),
            nix::unistd::geteuid().as_raw(),
        ) {
            tracing::warn!(plugin_id = %self.plugin_id, peer_uid = ?peer_uid, "plugin socket peer uid refused");
            send_error(
                &mut write_half,
                "-",
                "peer uid is not this plugin's account",
            )
            .await?;
            return Ok(None);
        }
        let Some(env) = read_envelope_capped(&mut read_half, HELLO_MAX_FRAME).await? else {
            return Ok(None); // clean EOF before any frame
        };
        if env.method != "hello" {
            send_error(&mut write_half, "-", "expected hello envelope").await?;
            return Ok(None);
        }
        let token = match CapabilityToken::from_token_string(&env.token) {
            Ok(t) => t,
            Err(e) => {
                send_error(
                    &mut write_half,
                    &env.request_id,
                    &format!("capability token invalid: {e}"),
                )
                .await?;
                return Ok(None);
            }
        };
        if let Err(e) = self.token_issuer.verify(&token, now_secs()) {
            send_error(
                &mut write_half,
                &env.request_id,
                &format!("capability token invalid: {e}"),
            )
            .await?;
            return Ok(None);
        }
        if token.plugin_id != self.plugin_id {
            send_error(
                &mut write_half,
                &env.request_id,
                &format!(
                    "token plugin_id {} does not match socket {}",
                    token.plugin_id, self.plugin_id
                ),
            )
            .await?;
            return Ok(None);
        }
        // ready handshake response: {"ready": true}
        send_response(
            &mut write_half,
            &env.request_id,
            Value::Map(vec![(Value::from("ready"), Value::Boolean(true))]),
        )
        .await?;
        Ok(Some((token, read_half, write_half)))
    }
}
