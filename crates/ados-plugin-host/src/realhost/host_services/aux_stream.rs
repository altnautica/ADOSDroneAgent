//! Radio aux-stream host methods and plugin/session release.

use super::*;

impl RealHost {
    pub(super) async fn serve_radio_aux_stream_open(
        &self,
        plugin_id: &str,
        session: u64,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        // The dispatch gate already enforced the auxiliary-stream capability. The
        // open carries no caller-tunable parameters: the radio service resolves the
        // effective aux ports / FEC / MCS from its own config, so the host never
        // lets a plugin pick a radio-port (which could collide with the data or
        // control planes).
        //
        // The stream is one shared resource, so ownership is claimed by this
        // connection BEFORE the forward: an open while another connection owns
        // it is refused without reaching the radio service, and two opening at
        // once cannot both come away as the owner. A claim the service then
        // refuses is released.
        let claimed = {
            let mut owner = self
                .aux_stream_owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match owner.as_ref() {
                Some((p, s)) if p == plugin_id && *s == session => false,
                Some(_) => {
                    return Err(HostError::Rpc(
                        "radio aux stream is open by another connection".to_string(),
                    ));
                }
                None => {
                    *owner = Some((plugin_id.to_string(), session));
                    true
                }
            }
        };
        let req = serde_json::json!({"op": "open"});
        let (reply, ok) = self.forward_radio_aux(req, "radio.aux_stream.open").await;
        if !ok && claimed {
            let mut owner = self
                .aux_stream_owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if owner
                .as_ref()
                .is_some_and(|(p, s)| p == plugin_id && *s == session)
            {
                *owner = None;
            }
        }
        Ok(reply)
    }

    pub(super) async fn serve_radio_aux_stream_close(
        &self,
        plugin_id: &str,
        session: u64,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        // Only the owning connection may close: the pair is shared with the
        // agent's own aux users, so a close from anyone else is refused before
        // it reaches the radio service. A confirmed close clears the ownership
        // record, so a later disconnect does not forward a redundant close.
        self.require_aux_owner(plugin_id, session)?;
        let req = serde_json::json!({"op": "close"});
        let (reply, ok) = self.forward_radio_aux(req, "radio.aux_stream.close").await;
        if ok {
            let mut owner = self
                .aux_stream_owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if owner
                .as_ref()
                .is_some_and(|(p, s)| p == plugin_id && *s == session)
            {
                *owner = None;
            }
        }
        Ok(reply)
    }

    pub(super) async fn serve_radio_aux_stream_send(
        &self,
        plugin_id: &str,
        session: u64,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        // The dispatch gate already enforced the auxiliary-stream capability. A
        // send carries an aux channel (AppStream 8 or AppCommand 9) plus the
        // application payload. The host validates the channel onto the two
        // reserved application channels, frames the payload as an aux frame
        // (the radio service forwards opaque app datagrams), and one-shots the
        // datagram to the radio service's command socket. Only the plugin that
        // opened the stream may transmit on it.
        let channel = map_get(args, "channel")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| HostError::Rpc("channel missing or not an integer".to_string()))?;
        // Checked on the full integer: narrowing first would wrap 264 onto 8.
        let channel = match channel {
            8 => 8u8,
            9 => 9u8,
            other => return Err(HostError::Rpc(format!("unsupported aux channel {other}"))),
        };
        let payload = map_get(args, "payload")
            .map(|v| coerce_msg_bytes(v).map_err(|_| "payload must be bytes".to_string()))
            .transpose()
            .map_err(HostError::Rpc)?
            .ok_or_else(|| HostError::Rpc("payload missing".to_string()))?;
        let aux_channel = ados_protocol::aux_mux::AuxChannel::from_u8(channel)
            .expect("channel validated to 8/9 above");
        let frame = ados_protocol::aux_mux::encode(aux_channel, &payload)
            .ok_or_else(|| HostError::Rpc("payload exceeds the aux frame maximum".to_string()))?;
        self.require_aux_owner(plugin_id, session)?;
        let req = serde_json::json!({"op": "send", "frame": frame});
        let (reply, _ok) = self.forward_radio_aux(req, "radio.aux_stream.send").await;
        Ok(reply)
    }

    pub(super) fn serve_radio_aux_stream_subscribe_stream(
        &self,
        plugin_id: &str,
        session: u64,
    ) -> Result<Option<crate::host::AuxDatagramRx>, HostError> {
        // Only the connection that opened the stream may listen to it: the
        // datagrams are that plugin's application traffic.
        self.require_aux_owner(plugin_id, session)?;
        // One connection to the radio service's aux command socket for the whole
        // host process, shared by every subscriber — mirrors the BUTTON_CLIENT
        // single-connection philosophy. Started lazily (per-instance, so a test
        // with an overridden `radio_aux_cmd_path` round-trips against its stub).
        //
        // Always `Some`: an absent aux pair (or a down radio service) is a normal
        // resting state, so the subscription succeeds and stays quiet while the
        // reader retries underneath. Returning `None` would make every plugin
        // treat "no app frames yet" as an error.
        let tx = self.aux_reader.get_or_init(|| {
            let (tx, _rx) = tokio::sync::broadcast::channel(AUX_BROADCAST_DEPTH);
            let path = self.radio_aux_cmd_path.clone();
            let task_tx = tx.clone();
            tokio::spawn(async move {
                crate::button_client::reconnect_forever("radio-aux", || {
                    aux_subscribe_pump(&path, &task_tx)
                })
                .await;
            });
            tx
        });
        Ok(Some(tx.subscribe()))
    }

    pub(super) async fn serve_release_plugin(&self, plugin_id: &str) {
        // A stopped plugin's page does not stay on the panel.
        let owned = {
            let mut owner = self
                .display_page_owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let owned = owner.as_ref().is_some_and(|(p, _)| p == plugin_id);
            if owned {
                *owner = None;
            }
            owned
        };
        if owned {
            let _ = std::fs::remove_file(&self.display_page_path);
        }
        // Its registered vision models leave the engine's registry with it. An
        // engine that is down holds no registry to clean.
        if let Some(client) = self.vision.as_ref() {
            if let Err(e) = client.unregister_owner(plugin_id).await {
                tracing::debug!(plugin_id, error = %e, "vision_unregister_owner_failed");
            }
        }
    }

    pub(super) async fn serve_release_session(&self, plugin_id: &str, session: u64) {
        // Component reservations carry the session that made them, so only
        // this session's are dropped; one a reconnect renewed stays. The
        // config store is deliberately NOT cleared (it persists across
        // reconnects).
        self.components
            .lock()
            .expect("components mutex poisoned")
            .release_session(plugin_id, session);
        // An mDNS record belongs to the connection that published it, so it is
        // withdrawn with exactly that session.
        self.mdns.release(plugin_id, session);
        // The display page is cleared when the connection that set it ends.
        let page_owned = {
            let mut owner = self
                .display_page_owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let owned = owner
                .as_ref()
                .is_some_and(|(p, s)| p == plugin_id && *s == session);
            if owned {
                *owner = None;
            }
            owned
        };
        if page_owned {
            let _ = std::fs::remove_file(&self.display_page_path);
        }
        // SAFE-by-default: a radio auxiliary stream never outlives the
        // connection that opened it, and no other connection's teardown closes
        // it. Take the owner slot first so the forward happens without holding
        // the lock.
        let owned = {
            let mut owner = self
                .aux_stream_owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let owned = owner
                .as_ref()
                .is_some_and(|(p, s)| p == plugin_id && *s == session);
            if owned {
                *owner = None;
            }
            owned
        };
        if owned {
            let req = serde_json::json!({"op": "close"});
            let _ = self.forward_radio_aux(req, "radio.aux_stream.close").await;
        }
    }
}
