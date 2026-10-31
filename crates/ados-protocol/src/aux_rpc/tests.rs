use super::*;
use std::time::{Duration, Instant};

fn parts<'a>(path: &'a [u8], body: &'a [u8]) -> RequestParts<'a> {
    RequestParts {
        path,
        content_type: b"",
        ticket: b"",
        body,
    }
}

/// Encode and reassemble in order, the happy path a radio delivers.
fn round_trip(method: RpcMethod, id: u32, target: &[u8], req: &RequestParts<'_>) -> RpcRequest {
    let frags = encode_request(method, id, target, req).unwrap();
    let mut r = RequestReassembler::new();
    let now = Instant::now();
    let mut done = None;
    for f in &frags {
        if let ReassemblyOutcome::Complete(req) = r.push(&decode_request_fragment(f).unwrap(), now)
        {
            done = Some(req);
        }
    }
    done.expect("the request completes")
}

#[test]
fn a_small_request_is_one_fragment_and_round_trips() {
    let req = RequestParts {
        path: b"/api/config",
        content_type: b"application/json",
        ticket: b"v2|tok",
        body: br#"{"key":"agent.name","value":"example-drone"}"#,
    };
    let frags = encode_request(RpcMethod::Put, 7, b"0a1b2c3d4e5f", &req).unwrap();
    assert_eq!(frags.len(), 1);
    assert!(frags[0].len() <= AUX_MAX_PAYLOAD);
    let got = round_trip(RpcMethod::Put, 7, b"0a1b2c3d4e5f", &req);
    assert_eq!(got.id, 7);
    assert_eq!(got.method, RpcMethod::Put);
    assert_eq!(got.target, b"0a1b2c3d4e5f");
    assert_eq!(got.path, req.path);
    assert_eq!(got.content_type, req.content_type);
    assert_eq!(got.ticket, req.ticket);
    assert_eq!(got.body, req.body);
}

#[test]
fn a_body_far_past_one_frame_fragments_and_reassembles_out_of_order() {
    // Plugin uploads and larger writes used to be refused with a 413.
    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let req = RequestParts {
        path: b"/api/plugins/com.example.tool/x/upload",
        content_type: b"application/octet-stream",
        ticket: b"",
        body: &body,
    };
    let frags = encode_request(RpcMethod::Patch, 9, b"d1", &req).unwrap();
    assert!(frags.len() > 100);
    assert!(frags.iter().all(|f| f.len() <= AUX_MAX_PAYLOAD));
    let mut r = RequestReassembler::new();
    let now = Instant::now();
    let mut order: Vec<usize> = (0..frags.len()).rev().collect();
    // A duplicate in the middle of the stream must not count twice.
    order.insert(5, order[3]);
    let mut done = None;
    for i in order {
        match r.push(&decode_request_fragment(&frags[i]).unwrap(), now) {
            ReassemblyOutcome::Complete(req) => done = Some(req),
            ReassemblyOutcome::Pending | ReassemblyOutcome::Duplicate => {}
            ReassemblyOutcome::Rejected => panic!("rejected"),
        }
    }
    let got = done.expect("complete");
    assert_eq!(got.method, RpcMethod::Patch);
    assert_eq!(got.body, body);
    assert_eq!(got.content_type, b"application/octet-stream");
    assert_eq!(r.partial_count(), 0);
}

#[test]
fn a_body_over_one_mebibyte_is_refused_at_encode() {
    let body = vec![0u8; MAX_REQUEST_BODY + 1];
    assert_eq!(
        encode_request(RpcMethod::Post, 1, b"d1", &parts(b"/api/x", &body)),
        Err(RequestEncodeError::BodyTooLarge)
    );
    let body = vec![0u8; MAX_REQUEST_BODY];
    assert!(encode_request(RpcMethod::Post, 1, b"d1", &parts(b"/api/x", &body)).is_ok());
}

#[test]
fn a_partial_request_is_dropped_after_the_reassembly_window() {
    let body = vec![1u8; 5_000];
    let frags = encode_request(RpcMethod::Post, 3, b"d1", &parts(b"/api/x", &body)).unwrap();
    let mut r = RequestReassembler::new();
    let t0 = Instant::now();
    assert_eq!(
        r.push(&decode_request_fragment(&frags[0]).unwrap(), t0),
        ReassemblyOutcome::Pending
    );
    let later = t0 + REQUEST_REASSEMBLY_TIMEOUT + Duration::from_secs(1);
    for f in &frags[1..] {
        assert_eq!(
            r.push(&decode_request_fragment(f).unwrap(), later),
            ReassemblyOutcome::Pending,
            "the stale first fragment is gone, so this cannot complete"
        );
    }
}

#[test]
fn concurrent_partials_are_bounded() {
    let body = vec![1u8; 3_000];
    let mut r = RequestReassembler::new();
    let now = Instant::now();
    for id in 0..MAX_PARTIAL_REQUESTS as u32 {
        let frags = encode_request(RpcMethod::Post, id, b"d1", &parts(b"/api/x", &body)).unwrap();
        assert_eq!(
            r.push(&decode_request_fragment(&frags[0]).unwrap(), now),
            ReassemblyOutcome::Pending
        );
    }
    let frags = encode_request(RpcMethod::Post, 999, b"d1", &parts(b"/api/x", &body)).unwrap();
    assert_eq!(
        r.push(&decode_request_fragment(&frags[0]).unwrap(), now),
        ReassemblyOutcome::Rejected
    );
}

#[test]
fn rejects_a_target_longer_than_a_device_id() {
    let too_long = vec![b'a'; MAX_DEVICE_ID + 1];
    assert_eq!(
        encode_request(RpcMethod::Get, 1, &too_long, &parts(b"/api/x", b"")),
        Err(RequestEncodeError::FieldTooLong)
    );
    let fits = vec![b'a'; MAX_DEVICE_ID];
    assert_eq!(
        round_trip(RpcMethod::Get, 1, &fits, &parts(b"/api/x", b"")).target,
        fits
    );
}

#[test]
fn rejects_a_truncated_fragment() {
    let frags = encode_request(RpcMethod::Post, 9, b"abc", &parts(b"/api/x", b"payload")).unwrap();
    assert_eq!(
        decode_request_fragment(&frags[0][..5]).unwrap_err(),
        RpcCodecError::TooShort
    );
}

#[test]
fn an_impossible_sequence_number_is_refused() {
    let mut frag =
        encode_request(RpcMethod::Get, 1, b"", &parts(b"/api/x", b"")).unwrap()[0].clone();
    // seq (bytes 6..8 with an empty target) set past total.
    frag[6..8].copy_from_slice(&3u16.to_be_bytes());
    assert_eq!(
        decode_request_fragment(&frag).unwrap_err(),
        RpcCodecError::BadFragmentIndex { index: 3, total: 1 }
    );
}

#[test]
fn an_object_whose_lengths_lie_is_rejected() {
    let mut frag =
        encode_request(RpcMethod::Get, 1, b"", &parts(b"/api/x", b"")).unwrap()[0].clone();
    // path_len (first two chunk bytes) claims more than is present.
    frag[10..12].copy_from_slice(&500u16.to_be_bytes());
    let mut r = RequestReassembler::new();
    assert_eq!(
        r.push(&decode_request_fragment(&frag).unwrap(), Instant::now()),
        ReassemblyOutcome::Rejected
    );
}

#[test]
fn rejects_an_unknown_method_byte() {
    let mut frag =
        encode_request(RpcMethod::Get, 1, b"", &parts(b"/api/x", b"")).unwrap()[0].clone();
    frag[0] = 0xFF;
    assert_eq!(
        decode_request_fragment(&frag).unwrap_err(),
        RpcCodecError::BadMethod(0xFF)
    );
}

#[test]
fn method_numbers_are_pinned() {
    // Renumbering would silently reroute a POST into a GET handler.
    assert_eq!(RpcMethod::Get as u8, 1);
    assert_eq!(RpcMethod::Post as u8, 2);
    assert_eq!(RpcMethod::Put as u8, 3);
    assert_eq!(RpcMethod::Delete as u8, 4);
    assert_eq!(RpcMethod::Patch as u8, 5);
    assert_eq!(RpcMethod::from_u8(5), Some(RpcMethod::Patch));
    assert_eq!(RpcMethod::from_u8(0), None);
    for m in [
        RpcMethod::Get,
        RpcMethod::Post,
        RpcMethod::Put,
        RpcMethod::Delete,
        RpcMethod::Patch,
    ] {
        assert_eq!(RpcMethod::from_http_method(m.as_http_method()), Some(m));
    }
    assert_eq!(RpcMethod::from_http_method("OPTIONS"), None);
}
