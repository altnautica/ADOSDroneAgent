//! The connection task: dial, read, and dispatch engine frames.

use super::*;

/// The connection task: connect, serve until the engine goes away, clear the
/// connection, wait the fixed interval, repeat. Forever.
pub(super) async fn run(shared: Arc<Shared>, path: PathBuf, interval: Duration) {
    // One line per outage, not one per attempt.
    let mut reported_absent = false;
    loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                reported_absent = false;
                let (read_half, write_half) = stream.into_split();
                let (resp_tx, resp_rx) = mpsc::channel::<Result<Value, String>>(64);
                shared
                    .set_connection(Some(RequestChannel {
                        write_half,
                        responses: resp_rx,
                    }))
                    .await;
                tracing::info!(path = %path.display(), "vision engine connected");

                // The reader and the re-arm run together: the re-arm's
                // subscribe requests need the reader to deliver their responses.
                // The re-arm then parks, so only the reader ends the select.
                let reader = read_loop(
                    read_half,
                    shared.frames.clone(),
                    shared.detections.clone(),
                    resp_tx,
                );
                let rearm = async {
                    shared.rearm(Push::Frames).await;
                    shared.rearm(Push::Detections).await;
                    std::future::pending::<()>().await
                };
                tokio::select! {
                    () = reader => {}
                    () = rearm => {}
                }

                shared.set_connection(None).await;
                tracing::warn!(
                    path = %path.display(),
                    "vision engine connection lost; reconnecting"
                );
            }
            Err(e) if !reported_absent => {
                reported_absent = true;
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "vision engine socket unavailable; retrying until it appears"
                );
            }
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "vision engine socket unavailable");
            }
        }
        tokio::time::sleep(interval).await;
    }
}

/// Drain the engine socket, routing each inbound envelope. `vision.deliver`
/// event envelopes carry a frame descriptor whose `descriptor` bytes are fanned
/// out on `frames`; every other envelope is a response and is forwarded on
/// `responses`. A clean EOF or a malformed/oversized header stops the loop and
/// drops `responses`, so a caller awaiting a response sees the close. The
/// fanouts stay open: subscribers resume on the next connection.
///
/// A push whose fanout has no receivers is dropped without being decoded; see
/// [`peek_push_kind`].
pub(super) async fn read_loop(
    mut read_half: OwnedReadHalf,
    frames: broadcast::Sender<Vec<u8>>,
    detections: broadcast::Sender<Vec<u8>>,
    responses: tokio::sync::mpsc::Sender<Result<Value, String>>,
) {
    loop {
        let mut header = [0u8; HEADER_SIZE];
        if read_half.read_exact(&mut header).await.is_err() {
            break;
        }
        let len = match decode_len(header, PLUGIN_MAX_FRAME, true) {
            Ok(n) => n,
            Err(_) => break,
        };
        let mut body = vec![0u8; len];
        if read_half.read_exact(&mut body).await.is_err() {
            break;
        }
        // Nobody holds a receiver on the fanout this push would feed: drop it
        // before the decode. Arming is process-lifetime and there is no engine
        // `vision.unsubscribe_frames`, so one short-lived plugin that
        // subscribed once would otherwise leave the host decoding every
        // camera's descriptors at capture rate for the rest of the process,
        // allocating the payload twice per frame for a `send` that can only
        // return `Err`. The probe is skipped entirely while both fanouts have
        // subscribers, so the fully-subscribed path pays nothing for it.
        if frames.receiver_count() == 0 || detections.receiver_count() == 0 {
            match peek_push_kind(&body) {
                PushKind::Frame if frames.receiver_count() == 0 => continue,
                PushKind::Detection if detections.receiver_count() == 0 => continue,
                _ => {}
            }
        }
        let env = match Envelope::from_msgpack(&body) {
            Ok(env) => env,
            Err(_) => break,
        };
        if env.method == methods::DELIVER_FRAME {
            if let Some(descriptor) = frame_descriptor_bytes(&env.args) {
                // A send with no receivers returns Err; that is fine, the next
                // subscriber resumes at the tail.
                let _ = frames.send(descriptor);
            }
            continue;
        }
        if env.method == methods::DELIVER_DETECTION {
            if let Some(batch) = detection_batch_bytes(&env.args) {
                let _ = detections.send(batch);
            }
            continue;
        }
        // A response: forward the error if set, else the args map. If the
        // receiver is gone the requester already moved on, so stop.
        let payload = match env.error {
            Some(msg) => Err(msg),
            None => Ok(env.args),
        };
        if responses.send(payload).await.is_err() {
            break;
        }
    }
}

/// Wall-clock unix milliseconds, used to tag each request id.
pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
