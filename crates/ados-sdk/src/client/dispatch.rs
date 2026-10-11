//! Envelope framing, event dispatch to callbacks, and argument helpers.

use super::*;

/// Read one length-prefixed msgpack envelope. `Ok(None)` on clean EOF before
/// the header. Uses `ados-protocol` framing (plugin contract: reject zero,
/// 4 MiB cap) — no wire re-implementation.
pub(super) async fn read_envelope<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> Result<Option<Envelope>, ClientError> {
    let mut header = [0u8; HEADER_SIZE];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(ClientError::Io(e)),
    }
    let len = decode_len(header, PLUGIN_MAX_FRAME, true)?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(Envelope::from_msgpack(&body)?))
}

/// Register a callback under a topic / message-name pattern. Mirrors the
/// `setdefault(pattern, []).append(callback)` of the Python client.
pub(super) fn register_callback(map: &CallbackMap, pattern: &str, callback: EventCallback) {
    map.lock()
        .expect("callback lock")
        .entry(pattern.to_string())
        .or_default()
        .push(callback);
}

/// Dispatch an `event` delivery to every callback whose pattern matches the
/// envelope's `topic`. Mirrors `_dispatch_event`.
pub(super) fn dispatch_event(map: &CallbackMap, env: &Envelope) {
    let Some(topic) = map_get_str(&env.args, "topic") else {
        return;
    };
    invoke_matching(map, topic, &env.args);
}

/// Replace the capability token this client presents with the host's freshly
/// minted one.
///
/// The host re-mints from authoritative state — on a permission change, and
/// proactively before the TTL — and gates the LIVE session on its own copy, so
/// dropping this push costs nothing until the connection breaks. What it costs
/// then is everything: `hello` presents the plugin's copy, the issuer has
/// already rotated past it, and the handshake is refused with
/// `capability token invalid`. One socket blip past the first rotation and the
/// plugin is off the bus for the rest of the flight.
///
/// A malformed or unparseable payload is ignored rather than adopted: keeping a
/// token that still authenticates beats replacing it with one that cannot over
/// a single bad frame.
pub(super) fn adopt_refreshed_token(slot: &Arc<Mutex<String>>, plugin_id: &str, env: &Envelope) {
    let Some(wire) = map_get_str(&env.args, "token").filter(|s| !s.is_empty()) else {
        tracing::warn!(plugin_id = %plugin_id, "plugin_token_refresh_malformed");
        return;
    };
    let parsed = match CapabilityToken::from_token_string(wire) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                plugin_id = %plugin_id,
                error = %e,
                "plugin_token_refresh_unparseable"
            );
            return;
        }
    };
    let mut held = slot.lock().expect("token lock");
    // A capability the operator revoked mid-flight is simply absent from the new
    // grant set. Naming it here is how a plugin author sees the cause, rather
    // than meeting it later as an unexplained capability_denied.
    let revoked: Vec<String> = CapabilityToken::from_token_string(held.as_str())
        .map(|old| {
            old.granted_caps
                .difference(&parsed.granted_caps)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    *held = wire.to_string();
    drop(held);
    tracing::info!(
        plugin_id = %plugin_id,
        expires_at = parsed.expires_at,
        revoked = ?revoked,
        "plugin_token_refreshed"
    );
}

/// Dispatch a `mavlink.deliver` to every callback whose pattern matches the
/// envelope's `msg_name`. Mirrors `_dispatch_mavlink`: the callback sees the
/// full args map (`msg_name`, `frame`, `timestamp_ms`).
pub(super) fn dispatch_mavlink(map: &CallbackMap, env: &Envelope) {
    let Some(msg_name) = map_get_str(&env.args, "msg_name") else {
        return;
    };
    invoke_matching(map, msg_name, &env.args);
}

/// Dispatch a `vision.deliver_detection` batch push.
///
/// This arm exists because the envelope carries **no `topic`**. Without it the
/// batch fell through to [`dispatch_event`], which returns early when the topic
/// lookup fails — so every detection a Rust plugin subscribed to was dropped
/// with no error and no log. Routing is by the batch's decoded `camera_id`,
/// mirroring the frame push: callbacks keyed on that camera plus the wildcard.
/// An undecodable batch is dropped rather than guessed at.
pub(super) fn dispatch_detection(map: &CallbackMap, env: &Envelope) {
    let Some(Value::Binary(blob)) = map_get(&env.args, "batch") else {
        return;
    };
    let Ok(batch) = ados_protocol::framebus::DetectionBatch::from_msgpack(&blob) else {
        return;
    };
    let matched: Vec<EventCallback> = {
        let guard = map.lock().expect("callback lock");
        guard
            .iter()
            .filter(|(key, _)| {
                key.as_str() == batch.camera_id.as_str() || key.as_str() == VISION_ANY_CAMERA
            })
            .flat_map(|(_, cbs)| cbs.iter().cloned())
            .collect()
    };
    for cb in matched {
        cb(env.args.clone());
    }
}

/// Dispatch a `button.deliver` press.
///
/// A press has no subject to filter on, so every registered callback fires. The
/// callback receives the envelope args; the press itself is under `press`.
pub(super) fn dispatch_button(map: &CallbackMap, env: &Envelope) {
    let matched: Vec<EventCallback> = {
        let guard = map.lock().expect("callback lock");
        guard.values().flat_map(|cbs| cbs.iter().cloned()).collect()
    };
    for cb in matched {
        cb(env.args.clone());
    }
}

/// Dispatch a `vision.deliver` frame push. The envelope carries the encoded
/// [`ados_protocol::framebus::FrameDescriptor`] in the `descriptor` binary arg
/// (no `topic`), so routing is by the descriptor's decoded `camera_id`: every
/// callback keyed on that camera id, plus the wildcard key, fires with the
/// envelope `args` map (the vision client decodes the descriptor again to read
/// the ring). A descriptor that cannot be decoded is dropped silently.
pub(super) fn dispatch_vision(map: &CallbackMap, env: &Envelope) {
    let Some(Value::Binary(blob)) = map_get(&env.args, "descriptor") else {
        return;
    };
    let Ok(descriptor) = ados_protocol::framebus::FrameDescriptor::from_msgpack(&blob) else {
        return;
    };
    // Snapshot the matched callbacks (exact camera id + the wildcard) under the
    // lock, then invoke outside it so a callback that re-subscribes cannot
    // deadlock. The two keys are exact strings, not globs, so no fnmatch here.
    let matched: Vec<EventCallback> = {
        let guard = map.lock().expect("callback lock");
        let mut out = Vec::new();
        if let Some(cbs) = guard.get(&descriptor.camera_id) {
            out.extend(cbs.iter().cloned());
        }
        if descriptor.camera_id != VISION_ANY_CAMERA {
            if let Some(cbs) = guard.get(VISION_ANY_CAMERA) {
                out.extend(cbs.iter().cloned());
            }
        }
        out
    };
    for cb in matched {
        cb(env.args.clone());
    }
}

/// Invoke every callback whose registered pattern equals `subject` or matches
/// it as a glob. Mirrors the `pattern == subject or fnmatch(...)` test in both
/// Python dispatchers.
pub(super) fn invoke_matching(map: &CallbackMap, subject: &str, args: &Value) {
    // Snapshot the matched callbacks under the lock, then invoke outside it so a
    // callback cannot deadlock by subscribing from within itself.
    let matched: Vec<EventCallback> = {
        let guard = map.lock().expect("callback lock");
        guard
            .iter()
            .filter(|(pattern, _)| pattern.as_str() == subject || fnmatch(pattern, subject))
            .flat_map(|(_, cbs)| cbs.iter().cloned())
            .collect()
    };
    for cb in matched {
        cb(args.clone());
    }
}

/// Map the host's error string to the typed [`ClientError`]. Mirrors the
/// `_send_request` error-prefix handling: `capability_denied:` and
/// `allowlist_violation:` get dedicated variants; everything else is `Rpc`.
pub(super) fn map_error_string(err: &str) -> ClientError {
    if let Some(rest) = err.strip_prefix("capability_denied:") {
        ClientError::CapabilityDenied(rest.trim().to_string())
    } else if let Some(rest) = err.strip_prefix("allowlist_violation:") {
        ClientError::AllowlistViolation(rest.trim().to_string())
    } else if err.contains("not permitted") {
        // The per-topic publish/subscribe refusal carries no machine prefix;
        // surface it as a capability denial like the Python client does.
        ClientError::CapabilityDenied(err.to_string())
    } else {
        ClientError::Rpc(err.to_string())
    }
}

// ---------------------------------------------------------------------------
// msgpack-map field readers (the args are an rmpv::Value::Map)
// ---------------------------------------------------------------------------

pub(super) fn map_get(args: &Value, key: &str) -> Option<Value> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v.clone()),
        _ => None,
    }
}

/// Slack on top of a browse window before the client gives up on the reply.
pub(super) const MDNS_BROWSE_REPLY_SLACK: Duration = Duration::from_secs(2);

/// A typed host reply, or the host's graceful-degrade `error` (an older host's
/// `not_implemented`) as [`ClientError::Rpc`].
pub(super) fn decode_typed_reply<T: serde::de::DeserializeOwned>(
    args: Value,
    method: &str,
) -> Result<T, ClientError> {
    if let Some(error) = map_get_str(&args, "error") {
        return Err(ClientError::Rpc(format!("{error}: {method}")));
    }
    rmpv::ext::from_value(args)
        .map_err(|e| ClientError::Rpc(format!("{method} reply did not decode: {e}")))
}

pub(super) fn map_get_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| v.as_str()),
        _ => None,
    }
}

/// The catalog capability a `peripheral.register_driver` call of `kind` needs:
/// camera and lidar have their own register caps; gimbal, GPS, ESC and payload
/// actuators register as payload devices. `None` for a kind the host has no
/// capability for.
pub fn driver_kind_capability(kind: &str) -> Option<&'static str> {
    match kind {
        "camera" => Some("sensor.camera.register"),
        "lidar" => Some("sensor.lidar.register"),
        "gimbal" | "gps" | "esc" | "payload-actuator" => Some("sensor.payload.register"),
        _ => None,
    }
}

pub(super) fn map_get_i64(args: &Value, key: &str) -> Option<i64> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| v.as_i64()),
        _ => None,
    }
}

/// Minimal glob matcher supporting `*` (any run, including across `.`) and `?`
/// (one char). Matches the host's `topic_matches` semantics for callback
/// routing, mirroring the Python client's `fnmatch.fnmatchcase`.
pub(super) fn fnmatch(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}
