//! Sending a command and correlating the FC's `COMMAND_ACK`.

use super::*;

/// Send `base_long` and correlate the FC's `COMMAND_ACK`.
///
/// Opens a dedicated MAVLink-socket connection per attempt, writes the command
/// (with `confirmation = attempt`), and reads the broadcast FC frame stream for a
/// matching `COMMAND_ACK`. On a timeout with no ACK it resends with
/// `confirmation++` up to the configured retry budget; on `IN_PROGRESS` it waits
/// (longer) for a final result and stops resending; on a rejection it attaches
/// any `STATUSTEXT` seen in the window. Returns [`AckOutcome::NoAck`] if the
/// retry window closes with no ACK. An absent socket / write failure on the FIRST
/// attempt is an `Err` the route maps to a 503.
///
/// ## Why the stream is re-opened per attempt
///
/// `AckStream::read_frame` bounds its read with `tokio::time::timeout` around
/// `read_exact`, which is not cancellation-safe: on expiry the future is dropped
/// having possibly consumed part of a length prefix or payload, so the byte
/// stream is at an unknown offset. Reusing it across a resend made every
/// subsequent frame parse against a shifted window, so `match_command_ack` missed
/// an ACK the FC really sent and this function reported `NoAck` for an ACCEPTED
/// arm / disarm / mode-set — the operator told a flight command failed when it
/// succeeded, and the retry re-sent it. The stream now marks itself desynced on
/// any timeout (refusing further use) and the attempt loop opens a fresh
/// connection, which is also the only way to resynchronise a raw byte stream.
///
/// A re-open that fails on a RETRY is not a 503: the first frame did reach the
/// socket, so the honest outcome is `NoAck` (sent, no ACK observed) rather than
/// "no MAVLink connection".
pub(super) async fn send_awaiting_ack(
    client: &MavlinkIpcClient,
    base_long: &COMMAND_LONG_DATA,
    cfg: &AckConfig,
) -> Result<AckOutcome, crate::ipc::mavlink_client::SendError> {
    let expected_cmd = base_long.command;
    let expected_src = base_long.target_system;

    for attempt in 0..=cfg.retries {
        // A fresh connection per attempt: the previous one may have been left at
        // an unknown byte offset by a cancelled bounded read.
        let mut stream = match client.open_ack_stream().await {
            Ok(s) => s,
            // The first attempt never wrote anything, so an unreachable socket is
            // the route's 503. A later attempt has already put a frame on the
            // wire; report that honestly instead.
            Err(e) if attempt == 0 => return Err(e),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    attempt,
                    "ack-stream re-open failed on a resend; reporting no-ack observed"
                );
                return Ok(AckOutcome::NoAck);
            }
        };

        // (Re)send with confirmation = attempt (0 on the first send).
        let mut long = base_long.clone();
        long.confirmation = attempt.min(u8::MAX as u32) as u8;
        let header = MavHeader {
            system_id: SOURCE_SYSTEM_ID,
            component_id: SOURCE_COMPONENT_ID,
            sequence: 0,
        };
        let frame =
            mavlink::serialize_v2(header, &MavMessage::COMMAND_LONG(long)).map_err(|e| {
                crate::ipc::mavlink_client::SendError::Io(std::io::Error::other(e.to_string()))
            })?;
        stream.write_frame(&frame).await?;

        let mut last_statustext: Option<String> = None;
        let mut in_progress = false;
        let mut deadline = Instant::now() + cfg.base_timeout;

        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match stream.read_frame(deadline - now).await {
                FrameRead::Frame(payload) => {
                    if let Some(result) = match_command_ack(&payload, expected_cmd, expected_src) {
                        if result == MAV_RESULT_IN_PROGRESS {
                            // Accepted and executing: wait longer for a final
                            // result and stop resending.
                            in_progress = true;
                            deadline = Instant::now() + cfg.inprogress_timeout;
                            continue;
                        }
                        let statustext = if result == MAV_RESULT_ACCEPTED {
                            None
                        } else {
                            last_statustext.take()
                        };
                        return Ok(AckOutcome::Acked { result, statustext });
                    } else if let Some(txt) = statustext_of(&payload) {
                        // Buffer the latest STATUSTEXT so a following rejection
                        // ACK can carry the human-readable reason.
                        last_statustext = Some(txt);
                    }
                }
                // Nothing arrived in the window: the read was cancelled, so this
                // stream is now desynced and unusable. Break to the next attempt,
                // which opens a fresh one (or give up after the budget).
                FrameRead::Timeout => break,
                // The stream closed; no more frames will arrive. An IN_PROGRESS
                // already seen is still the FC's answer: accepted and executing.
                FrameRead::Eof if in_progress => {
                    return Ok(AckOutcome::Acked {
                        result: MAV_RESULT_IN_PROGRESS,
                        statustext: None,
                    })
                }
                FrameRead::Eof => return Ok(AckOutcome::NoAck),
            }
        }

        if in_progress {
            // We saw IN_PROGRESS but the extended wait elapsed with no final
            // result. Report the accepted-and-executing state; do not resend.
            return Ok(AckOutcome::Acked {
                result: MAV_RESULT_IN_PROGRESS,
                statustext: None,
            });
        }
        // else: timed out with no ACK → loop to resend with confirmation++ on a
        // fresh connection, unless the retry budget is exhausted.
    }

    Ok(AckOutcome::NoAck)
}

/// If `frame` is a `COMMAND_ACK` for `expected_cmd` from `expected_src_system`,
/// return its `MAV_RESULT` (as a `u8`). Correlation uses the acknowledged command
/// id AND the ACK's SOURCE system == the vehicle the command addressed — the
/// typed `COMMAND_ACK` in this dialect carries no `target_system` extension
/// field, so this source-system fallback is the correlation key. A frame that is
/// not a parseable `COMMAND_ACK`, is for another command, or is from another
/// system returns `None`.
pub(super) fn match_command_ack(
    frame: &[u8],
    expected_cmd: MavCmd,
    expected_src_system: u8,
) -> Option<u8> {
    let (header, msg) = mavlink::parse_any(frame).ok()?;
    match msg {
        MavMessage::COMMAND_ACK(ack)
            if ack.command == expected_cmd && header.system_id == expected_src_system =>
        {
            Some(ack.result as u8)
        }
        _ => None,
    }
}

/// If `frame` is a `STATUSTEXT`, return its text (trailing NULs trimmed). Used to
/// surface the human-readable reason for a command rejection.
pub(super) fn statustext_of(frame: &[u8]) -> Option<String> {
    let (_header, msg) = mavlink::parse_any(frame).ok()?;
    match msg {
        MavMessage::STATUSTEXT(s) => {
            let end = s.text.iter().position(|&b| b == 0).unwrap_or(s.text.len());
            let text = String::from_utf8_lossy(&s.text[..end]).trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

/// The human name for a `MAV_RESULT` value (common.xml).
pub(super) fn mav_result_name(result: u8) -> &'static str {
    match result {
        0 => "ACCEPTED",
        1 => "TEMPORARILY_REJECTED",
        2 => "DENIED",
        3 => "UNSUPPORTED",
        4 => "FAILED",
        5 => "IN_PROGRESS",
        6 => "CANCELLED",
        _ => "UNKNOWN",
    }
}

/// Merge the ACK outcome into the response body under an `ack` key. Accepted →
/// `{"observed":true,"result":0,"result_name":"ACCEPTED","accepted":true}`; a
/// rejection carries the result + name + any `statustext`; no ACK →
/// `{"observed":false}` (honest, never a fabricated result).
pub(super) fn merge_ack(body: &mut Value, outcome: &AckOutcome) {
    let ack = match outcome {
        AckOutcome::Acked { result, statustext } => {
            let mut m = json!({
                "observed": true,
                "result": result,
                "result_name": mav_result_name(*result),
                "accepted": *result == MAV_RESULT_ACCEPTED,
            });
            if let Some(txt) = statustext {
                m["statustext"] = json!(txt);
            }
            m
        }
        AckOutcome::NoAck => json!({ "observed": false }),
    };
    if let Value::Object(map) = body {
        map.insert("ack".to_string(), ack);
    }
}

/// Settle the body's top-level `status` from the FC's answer. The body is built
/// as `"status": "ok"` before the send; only an ACCEPTED ACK keeps it. A
/// rejection, an in-progress without a final result, or no ACK at all becomes
/// `"status": "error"` with a `message`, the body-level failure convention the
/// cloud dispatcher and other consumers read, so a DENIED arm is never reported
/// as a completed command. The `ack` block keeps the full detail.
pub(super) fn settle_status(body: &mut Value, outcome: &AckOutcome) {
    let message = match outcome {
        AckOutcome::Acked { result, .. } if *result == MAV_RESULT_ACCEPTED => return,
        AckOutcome::Acked { result, statustext } => statustext
            .clone()
            .unwrap_or_else(|| format!("flight controller answered {}", mav_result_name(*result))),
        AckOutcome::NoAck => "no acknowledgement from the flight controller".to_string(),
    };
    if let Value::Object(map) = body {
        map.insert("status".to_string(), json!("error"));
        map.insert("message".to_string(), json!(message));
    }
}
