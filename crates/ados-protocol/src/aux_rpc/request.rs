//! Request encoding and fragment decoding.

use super::*;

/// Encode a request as the aux payloads (the bytes that go inside each aux
/// frame AFTER the 6-byte aux header) that carry it, in send order.
///
/// `target` is the device id the request is addressed to; an empty slice is a
/// broadcast every linked drone answers.
pub fn encode_request(
    method: RpcMethod,
    id: u32,
    target: &[u8],
    request: &RequestParts<'_>,
) -> Result<Vec<Vec<u8>>, RequestEncodeError> {
    if request.body.len() > MAX_REQUEST_BODY {
        return Err(RequestEncodeError::BodyTooLarge);
    }
    if target.len() > MAX_DEVICE_ID
        || request.path.len() > u16::MAX as usize
        || request.content_type.len() > u8::MAX as usize
        || request.ticket.len() > MAX_REQUEST_TICKET
    {
        return Err(RequestEncodeError::FieldTooLong);
    }
    let mut object = Vec::with_capacity(
        5 + request.path.len()
            + request.content_type.len()
            + request.ticket.len()
            + request.body.len(),
    );
    object.extend_from_slice(&(request.path.len() as u16).to_be_bytes());
    object.extend_from_slice(request.path);
    object.push(request.content_type.len() as u8);
    object.extend_from_slice(request.content_type);
    object.extend_from_slice(&(request.ticket.len() as u16).to_be_bytes());
    object.extend_from_slice(request.ticket);
    object.extend_from_slice(request.body);

    let chunks: Vec<&[u8]> = object.chunks(MAX_REQUEST_CHUNK).collect();
    let total = chunks.len() as u16;
    Ok(chunks
        .iter()
        .enumerate()
        .map(|(seq, chunk)| {
            let mut out =
                Vec::with_capacity(RPC_REQUEST_FRAGMENT_OVERHEAD + target.len() + chunk.len());
            out.push(method as u8);
            out.extend_from_slice(&id.to_be_bytes());
            out.push(target.len() as u8);
            out.extend_from_slice(target);
            out.extend_from_slice(&(seq as u16).to_be_bytes());
            out.extend_from_slice(&total.to_be_bytes());
            out.extend_from_slice(chunk);
            out
        })
        .collect())
}

/// The parts of a request that travel inside its object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestParts<'a> {
    pub path: &'a [u8],
    pub content_type: &'a [u8],
    pub ticket: &'a [u8],
    pub body: &'a [u8],
}

/// Decode one request fragment.
pub fn decode_request_fragment(payload: &[u8]) -> Result<RequestFragment<'_>, RpcCodecError> {
    if payload.len() < RPC_REQUEST_FRAGMENT_OVERHEAD {
        return Err(RpcCodecError::TooShort);
    }
    let method = RpcMethod::from_u8(payload[0]).ok_or(RpcCodecError::BadMethod(payload[0]))?;
    let id = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let target_len = payload[5] as usize;
    let seq_offset = 6 + target_len;
    if seq_offset + 4 > payload.len() {
        return Err(RpcCodecError::LengthMismatch {
            declared: seq_offset + 4,
            actual: payload.len(),
        });
    }
    let target = &payload[6..seq_offset];
    let seq = u16::from_be_bytes([payload[seq_offset], payload[seq_offset + 1]]);
    let total = u16::from_be_bytes([payload[seq_offset + 2], payload[seq_offset + 3]]);
    if total == 0 || seq >= total || total as usize > MAX_REQUEST_FRAGMENTS {
        return Err(RpcCodecError::BadFragmentIndex { index: seq, total });
    }
    Ok(RequestFragment {
        id,
        method,
        target,
        seq,
        total,
        chunk: &payload[seq_offset + 4..],
    })
}

/// Split a reassembled request object into its fields.
pub(super) fn decode_request_object(
    id: u32,
    method: RpcMethod,
    target: &[u8],
    object: &[u8],
) -> Result<RpcRequest, RpcCodecError> {
    let mut at = 0usize;
    let take = |at: &mut usize, n: usize| -> Result<std::ops::Range<usize>, RpcCodecError> {
        let end = at.checked_add(n).filter(|end| *end <= object.len()).ok_or(
            RpcCodecError::LengthMismatch {
                declared: at.saturating_add(n),
                actual: object.len(),
            },
        )?;
        let range = *at..end;
        *at = end;
        Ok(range)
    };
    let r = take(&mut at, 2)?;
    let path_len = u16::from_be_bytes([object[r.start], object[r.start + 1]]) as usize;
    let path = take(&mut at, path_len)?;
    let r = take(&mut at, 1)?;
    let ct_len = object[r.start] as usize;
    let content_type = take(&mut at, ct_len)?;
    let r = take(&mut at, 2)?;
    let ticket_len = u16::from_be_bytes([object[r.start], object[r.start + 1]]) as usize;
    if ticket_len > MAX_REQUEST_TICKET {
        return Err(RpcCodecError::LengthMismatch {
            declared: ticket_len,
            actual: MAX_REQUEST_TICKET,
        });
    }
    let ticket = take(&mut at, ticket_len)?;
    let body = &object[at..];
    if body.len() > MAX_REQUEST_BODY {
        return Err(RpcCodecError::LengthMismatch {
            declared: body.len(),
            actual: MAX_REQUEST_BODY,
        });
    }
    Ok(RpcRequest {
        id,
        method,
        target: target.to_vec(),
        path: object[path].to_vec(),
        content_type: object[content_type].to_vec(),
        ticket: object[ticket].to_vec(),
        body: body.to_vec(),
    })
}
