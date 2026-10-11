//! Envelope framing on the plugin socket.

use super::*;

/// Read one length-prefixed msgpack envelope. `Ok(None)` on clean EOF before
/// the header. Uses `ados-protocol` framing (plugin contract: reject zero,
/// 4 MiB cap) — no wire re-implementation.
pub(super) async fn read_envelope<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> Result<Option<Envelope>, ServerError> {
    read_envelope_capped(reader, PLUGIN_MAX_FRAME).await
}

/// [`read_envelope`] with a tighter frame cap. The body is read as it
/// arrives rather than into a buffer sized from the header, so a peer that
/// announces a large frame and stalls holds no more memory than it sent.
pub(super) async fn read_envelope_capped<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    max_frame: usize,
) -> Result<Option<Envelope>, ServerError> {
    let mut header = [0u8; HEADER_SIZE];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = decode_len(header, max_frame, true)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let mut body = Vec::with_capacity(len.min(READ_CHUNK));
    let read = (&mut *reader)
        .take(len as u64)
        .read_to_end(&mut body)
        .await?;
    if read != len {
        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
    }
    let env = Envelope::from_msgpack(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(Some(env))
}

/// Write one envelope as a length-prefixed frame and flush.
pub(super) async fn write_frame<W: AsyncWriteExt + Unpin>(
    write_half: &mut W,
    env: &Envelope,
) -> Result<(), ServerError> {
    let frame = env
        .encode_frame()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    write_half.write_all(&frame).await?;
    write_half.flush().await?;
    Ok(())
}

/// Send a `response` envelope carrying `result` as its args.
pub(super) async fn send_response<W: AsyncWriteExt + Unpin>(
    write_half: &mut W,
    request_id: &str,
    result: Value,
) -> Result<(), ServerError> {
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: "response".to_string(),
        capability: String::new(),
        args: result,
        request_id: request_id.to_string(),
        token: String::new(),
        error: None,
    };
    write_frame(write_half, &env).await
}

/// Send a `response` envelope with the `error` field set.
pub(super) async fn send_error<W: AsyncWriteExt + Unpin>(
    write_half: &mut W,
    request_id: &str,
    message: &str,
) -> Result<(), ServerError> {
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: "response".to_string(),
        capability: String::new(),
        args: Value::Map(vec![]),
        request_id: request_id.to_string(),
        token: String::new(),
        error: Some(message.to_string()),
    };
    write_frame(write_half, &env).await
}
