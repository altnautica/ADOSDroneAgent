use super::*;

/// The measured `/api/services` size on a live drone — the endpoint whose
/// 413 was the reason fragmentation exists.
const MEASURED_SERVICES_BYTES: usize = 2631;

/// This drone's device id, stamped on every fragment it emits.
const OWN_ID: &[u8] = b"0a1b2c3d4e5f";

/// Rebuild a response from the fragments the handler produced, the way the
/// ground station does — including splitting the encoded object back into
/// its status, body and headers.
fn reassemble_full(
    frags: &[Vec<u8>],
    skip: &[usize],
) -> Option<(u16, Vec<aux_rpc::ResponseHeader>, Vec<u8>)> {
    let mut decoder: Option<aux_rpc::ResponseDecoder> = None;
    for (i, payload) in frags.iter().enumerate() {
        if skip.contains(&i) {
            continue;
        }
        let dec = aux_rpc::decode_response(payload).unwrap();
        assert_eq!(
            dec.sender, OWN_ID,
            "every fragment names the answering drone"
        );
        assert_eq!(dec.index, i as u16);
        assert_eq!(dec.total, frags.len() as u16);
        let d = decoder.get_or_insert_with(|| aux_rpc::ResponseDecoder::new(dec.oti).unwrap());
        if let aux_rpc::FragmentOutcome::Complete(object) = d.push(dec.index, dec.body) {
            let (status, body, headers) = aux_rpc::unpack_response(dec.status, object);
            return Some((status, headers, body));
        }
    }
    None
}

/// The common case: status + body, with the headers asserted separately by
/// the tests that care about them.
fn reassemble(frags: &[Vec<u8>], skip: &[usize]) -> Option<(u16, Vec<u8>)> {
    reassemble_full(frags, skip).map(|(status, _, body)| (status, body))
}

/// `start_paused` lets the bounded wait expire instantly instead of the
/// test sitting through the real limit.

#[tokio::test]
async fn a_response_larger_than_the_ceiling_stops_being_read_at_the_ceiling() {
    // The read used to run to EOF, bounded only by a 5 s timeout against
    // loopback, and the size check happened after the body was fully
    // buffered. With one task per relay request, N concurrent calls to a
    // large-response route each held their own copy.
    let (mut near, far) = tokio::io::duplex(64 * 1024);
    let ceiling = ados_protocol::aux_rpc::MAX_RESPONSE_BODY;
    // Far more than the ceiling, written for as long as anyone reads.
    tokio::spawn(async move {
        let chunk = vec![b'x'; 32 * 1024];
        loop {
            if near.write_all(&chunk).await.is_err() {
                break;
            }
        }
    });

    let got = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_response_bounded(far),
    )
    .await
    .expect("the read never terminated; it is still bounded only by a timeout")
    .expect("read failed");

    assert!(
        got.len() <= ceiling + HTTP_HEADER_ALLOWANCE + 1,
        "buffered {} bytes for a ceiling of {}",
        got.len(),
        ceiling
    );
}

#[tokio::test]
async fn a_response_under_the_ceiling_is_read_whole() {
    // The bound must not truncate the ordinary case.
    let (mut near, far) = tokio::io::duplex(64 * 1024);
    let body = b"HTTP/1.1 200 OK\r\n\r\n{\"ok\":true}".to_vec();
    let expected = body.clone();
    tokio::spawn(async move {
        let _ = near.write_all(&body).await;
    });
    let got = read_response_bounded(far).await.expect("read failed");
    assert_eq!(got, expected);
}

#[tokio::test(start_paused = true)]
async fn a_response_is_abandoned_rather_than_sent_after_the_caller_gave_up() {
    // The send slot is process-wide and held for a whole burst, so
    // concurrent responses queue linearly. That wait used to count against
    // nothing: a response could sit past the ground's call bound and
    // transmit anyway, spending uplink airtime on an answer nobody was
    // waiting for while this side recorded a success.
    let counters = AuxUplinkConsumerCounters::new();
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = sock.local_addr().unwrap();
    sock.connect(target).await.unwrap();
    let egress = AuxEgress::connected_for_test(sock);
    let fragments = encode_fragments(OWN_ID, 1, 200, &[], br#"{"ok":true}"#);

    // Hold the slot for longer than a caller would wait.
    let held = RESPONSE_SEND_SLOT.acquire().await.expect("slot");
    send_fragments(1, &egress, &fragments, &counters, Instant::now()).await;
    drop(held);

    assert_eq!(
        counters.snapshot().rpc_response_abandoned,
        1,
        "a response that waited past the caller's bound must be abandoned and counted"
    );
}

#[test]
fn the_send_slot_wait_stays_inside_the_grounds_call_bound() {
    // Waiting longer than the caller does turns a queued response into
    // wasted airtime: the ground has already timed out and retransmitted.
    assert!(
        SEND_SLOT_WAIT_LIMIT < ados_protocol::aux_rpc_proxy::RPC_DEFAULT_TIMEOUT,
        "the send-slot wait must expire before the ground gives up"
    );
    // And it must leave room for the burst itself to finish afterwards.
    assert!(
        SEND_SLOT_WAIT_LIMIT + Duration::from_millis(500)
            < ados_protocol::aux_rpc_proxy::RPC_DEFAULT_TIMEOUT,
        "the wait must leave the burst time to transmit"
    );
}

#[test]
fn a_small_body_travels_as_one_symbol_plus_its_repair_set() {
    let frags = encode_fragments(OWN_ID, 7, 200, &[], br#"{"ok":true}"#);
    assert_eq!(frags.len(), 1 + aux_rpc::RPC_REPAIR_SYMBOLS as usize);
    assert_eq!(
        reassemble(&frags, &[]),
        Some((200, br#"{"ok":true}"#.to_vec()))
    );
}

#[test]
fn a_services_sized_body_fragments_and_reassembles() {
    let body: Vec<u8> = (0..MEASURED_SERVICES_BYTES)
        .map(|i| (i % 251) as u8)
        .collect();
    let frags = encode_fragments(OWN_ID, 7, 200, &[], &body);
    let systematic = MEASURED_SERVICES_BYTES.div_ceil(aux_rpc::MAX_RESPONSE_FRAGMENT);
    assert_eq!(
        frags.len(),
        systematic + aux_rpc::RPC_REPAIR_SYMBOLS as usize
    );
    assert_eq!(reassemble(&frags, &[]), Some((200, body.clone())));
    // The repair symbols are the point: losing any two still answers.
    assert_eq!(reassemble(&frags, &[0, 2]), Some((200, body)));
}

#[test]
fn an_empty_body_still_travels_as_one_fragment() {
    let frags = encode_fragments(OWN_ID, 7, 204, &[], b"");
    assert_eq!(frags.len(), 1, "a 204 has no symbols to protect");
    let dec = aux_rpc::decode_response(&frags[0]).unwrap();
    assert_eq!(dec.status, 204);
    assert_eq!(dec.sender, OWN_ID);
    assert!(dec.body.is_empty());
    assert_eq!(reassemble(&frags, &[]), Some((204, Vec::new())));
}

#[test]
fn a_body_past_the_ceiling_becomes_a_413() {
    let body = vec![b'x'; 90_000];
    let frags = encode_fragments(OWN_ID, 7, 200, &[], &body);
    assert_eq!(
        reassemble(&frags, &[]),
        Some((
            413,
            b"response exceeds the relay reassembly ceiling".to_vec()
        )),
        "never truncate a body into a fake 200"
    );
}

#[test]
fn a_request_with_a_body_declares_json_and_one_without_does_not() {
    let write = build_request_head("PUT", "/api/config", "", 40);
    assert!(
        write.contains("Content-Type: application/json\r\n"),
        "axum Json<T> rejects a body with no content-type before the handler runs"
    );
    assert!(write.starts_with("PUT /api/config HTTP/1.1\r\n"));
    assert!(write.contains("Content-Length: 40\r\n"));

    let read = build_request_head("GET", "/api/logs?limit=5", "", 0);
    assert!(!read.contains("Content-Type"));
    assert!(read.starts_with("GET /api/logs?limit=5 HTTP/1.1\r\n"));
}

#[test]
fn the_callers_content_type_crosses_and_an_injected_one_does_not() {
    let upload = build_request_head("PATCH", "/api/plugins/p/x/f", "application/octet-stream", 9);
    assert!(upload.starts_with("PATCH /api/plugins/p/x/f HTTP/1.1\r\n"));
    assert!(upload.contains("Content-Type: application/octet-stream\r\n"));
    let typed = build_request_head("POST", "/x", "text/csv; charset=utf-8", 3);
    assert!(typed.contains("Content-Type: text/csv; charset=utf-8\r\n"));
    let injected = build_request_head("POST", "/x", "text/plain\r\nX-Injected: 1", 3);
    assert!(!injected.contains("X-Injected"));
    assert!(injected.contains("Content-Type: application/json\r\n"));
}

#[test]
fn the_allow_listed_response_headers_are_collected_and_the_rest_are_not() {
    let raw = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/csv; charset=utf-8\r\n\
Content-Disposition: attachment; filename=\"log.csv\"\r\n\
Location: /whep/session/abc\r\n\
Cache-Control: no-store\r\n\
Server: hyper\r\n\
Set-Cookie: sid=secret\r\n\
Content-Length: 3\r\n\r\nabc";
    let (status, headers, body) = parse_response(raw).unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, b"abc");
    assert_eq!(
        headers,
        vec![
            (
                "content-type".to_string(),
                "text/csv; charset=utf-8".to_string()
            ),
            (
                "content-disposition".to_string(),
                "attachment; filename=\"log.csv\"".to_string()
            ),
            ("location".to_string(), "/whep/session/abc".to_string()),
            ("cache-control".to_string(), "no-store".to_string()),
        ],
        "only the allow-list crosses, lowercased, in wire order"
    );
    // Every byte of header is a byte of body that cannot travel, and a
    // cookie is a credential with no business on a shared radio.
    assert!(headers
        .iter()
        .all(|(n, _)| n != "set-cookie" && n != "server"));
    // Rebuilt by the ground, never forwarded: the body it reassembles is
    // the one whose length counts.
    assert!(headers.iter().all(|(n, _)| n != "content-length"));
}

#[test]
fn a_content_type_reaches_the_ground_through_the_fragment_codec() {
    // The end-to-end property GA-01 is about: an artifact body that leaves
    // the drone typed arrives at the ground still typed.
    let body: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    let headers = vec![(
        "content-type".to_string(),
        "application/octet-stream".to_string(),
    )];
    let frags = encode_fragments(OWN_ID, 7, 200, &headers, &body);
    let (status, got_headers, got_body) =
        reassemble_full(&frags, &[1]).expect("repair symbols cover one loss");
    assert_eq!(status, 200, "the header flag never leaks into the status");
    assert_eq!(got_body, body);
    assert_eq!(got_headers, headers);
}

#[test]
fn a_headerless_answer_puts_the_same_bytes_on_the_air_as_before() {
    // The mixed-fleet guarantee: a drone on this build answering a relayed
    // JSON read emits exactly what a drone without header support emits,
    // so a ground station that has not upgraded keeps working.
    let body = br#"{"ok":true}"#;
    let with_support = encode_fragments(OWN_ID, 7, 200, &[], body);
    let dec = aux_rpc::decode_response(&with_support[0]).unwrap();
    assert_eq!(
        dec.status & aux_rpc::RESPONSE_HEADERS_FLAG,
        0,
        "no flag set when nothing is carried"
    );
    assert_eq!(reassemble(&with_support, &[]), Some((200, body.to_vec())));
}

#[test]
fn parses_a_200_response_with_a_body() {
    let raw =
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 5\r\n\r\nhello";
    let (status, _headers, body) = parse_response(raw).unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, b"hello");
}

#[test]
fn parses_a_404_response_with_an_empty_body() {
    let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
    let (status, _headers, body) = parse_response(raw).unwrap();
    assert_eq!(status, 404);
    assert!(body.is_empty());
}

#[test]
fn parses_a_500_response_with_a_long_body() {
    let body = vec![b'x'; 500];
    let raw = format!(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 500\r\n\r\n{}",
        String::from_utf8(body.clone()).unwrap()
    );
    let (status, _headers, parsed_body) = parse_response(raw.as_bytes()).unwrap();
    assert_eq!(status, 500);
    assert_eq!(parsed_body, body);
}

#[test]
fn rejects_a_response_with_no_header_boundary() {
    let raw = b"HTTP/1.1 200 OK";
    assert!(parse_response(raw).is_err());
}

#[test]
fn rejects_a_malformed_status_line() {
    let raw = b"NOT HTTP\r\n\r\nbody";
    assert!(parse_response(raw).is_err());
}

#[test]
fn handles_a_body_that_contains_the_boundary_pattern() {
    // The body itself contains \r\n\r\n — only the FIRST boundary splits.
    let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\na\r\n\r\nb";
    let (status, _headers, body) = parse_response(raw).unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, b"a\r\n\r\nb");
}

#[test]
fn the_relayed_marker_rides_every_request_head() {
    let head = build_request_head("GET", "/api/version", "", 0);
    assert!(
        head.contains("X-ADOS-Relayed: 1\r\n"),
        "attribution only: the key-gated ground route is the auth control, not this header"
    );
}

#[test]
fn a_path_carrying_control_characters_is_refused() {
    assert!(path_is_safe("/api/logs?limit=5&since=2026-01-01T00:00:00Z"));
    assert!(
        !path_is_safe("/api/version\r\nX-Injected: 1"),
        "a decoded %0D%0A would end the request line and inject a header"
    );
    assert!(!path_is_safe("/api/version\nX-Injected: 1"));
    assert!(!path_is_safe("/api/version\u{7f}"));
    assert!(!path_is_safe("/api/v1/network/configured/Home Net"));
    assert!(path_is_safe("/api/v1/network/configured/Home%20Net"));
}

#[test]
fn a_chunked_response_is_refused_as_a_bad_gateway() {
    let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
    let err = parse_response(raw).unwrap_err();
    assert!(matches!(err, HttpError::Chunked));
    assert_eq!(
        err.status(),
        502,
        "chunk framing forwarded as a body reaches the GCS as unparseable JSON"
    );
    assert_eq!(
        HttpError::Timeout.status(),
        503,
        "a wedged local API stays a 503"
    );
}

#[test]
fn a_declared_content_length_bounds_the_body() {
    let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello-and-then-some";
    let (status, _headers, body) = parse_response(raw).unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        body, b"hello",
        "bytes past the declared length are not part of this body"
    );

    let short = b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\nhello";
    assert_eq!(
        parse_response(short).unwrap().2,
        b"hello",
        "a server that closed early still yields what it did send"
    );
}

#[test]
fn one_unencodable_fragment_cancels_the_whole_response() {
    // MAX_RESPONSE_FRAGMENT budgets a worst-case device id, so a symbol one
    // byte past it is the first that cannot be framed for a sender whose id
    // is actually that long. A shorter id leaves slack and would still fit.
    let worst_case_sender = vec![b'a'; ados_protocol::node_status::MAX_DEVICE_ID];
    let oversized = aux_rpc::ResponseSymbols {
        oti: 8,
        symbols: vec![
            b"first".to_vec(),
            vec![0u8; aux_rpc::MAX_RESPONSE_FRAGMENT + 1],
        ],
    };
    assert_eq!(
        encode_all(&worst_case_sender, 7, 200, &oversized),
        Vec::<Vec<u8>>::new(),
        "a short send whose total still says 2 makes the ground wait out its call bound"
    );
    let fits = aux_rpc::ResponseSymbols {
        oti: 11,
        symbols: vec![b"first!".to_vec(), b"second".to_vec()],
    };
    assert_eq!(
        encode_all(OWN_ID, 7, 200, &fits).len(),
        2,
        "the all-or-nothing guard must not reject an encodable response"
    );
}

mod relay_authorization {
    use super::super::{authorize, SPENT_TICKETS};
    use ados_protocol::aux_rpc::{RpcMethod, RpcRequest};
    use ados_protocol::relay_ticket::{RelayTicketError, RelayTicketIssuer, RequestBinding};

    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const ME: &str = "0a1b2c3d4e5f";
    const NOW: i64 = 1_800_000_000;
    /// A path with no special standing, used wherever the decision must not
    /// depend on which route was called.
    const ORDINARY: &[u8] = b"/api/config";
    const BOOTSTRAP: &[u8] = b"/api/relay/peer-secret";

    fn request(method: RpcMethod, path: &[u8], body: &[u8], ticket: &str) -> RpcRequest {
        RpcRequest {
            id: 1,
            method,
            target: ME.as_bytes().to_vec(),
            path: path.to_vec(),
            content_type: Vec::new(),
            ticket: ticket.as_bytes().to_vec(),
            body: body.to_vec(),
        }
    }

    /// A ticket for `target`, minted by the holder of `secret` for exactly
    /// this method, path and body.
    fn ticket(
        secret: &str,
        target: &str,
        method: RpcMethod,
        path: &[u8],
        body: &[u8],
        at: i64,
    ) -> String {
        RelayTicketIssuer::from_secret(secret.as_bytes())
            .mint(
                target,
                &RequestBinding::new(
                    method.as_http_method(),
                    std::str::from_utf8(path).unwrap(),
                    body,
                ),
                at,
            )
            .unwrap()
    }

    fn ticketed(method: RpcMethod, path: &[u8], body: &[u8]) -> RpcRequest {
        request(
            method,
            path,
            body,
            &ticket(SECRET, ME, method, path, body, NOW),
        )
    }

    #[test]
    fn a_drone_with_no_secret_refuses_every_relayed_call() {
        // A relayed request lands on loopback and is treated as on-box, so
        // an aircraft with no credential to check must refuse rather than
        // hand radio range its full authority.
        for method in [
            RpcMethod::Get,
            RpcMethod::Post,
            RpcMethod::Put,
            RpcMethod::Delete,
            RpcMethod::Patch,
        ] {
            assert_eq!(
                authorize(None, &request(method, ORDINARY, b"", ""), ME, NOW),
                Err(RelayTicketError::NoSecret)
            );
            assert_eq!(
                authorize(None, &ticketed(method, ORDINARY, b""), ME, NOW),
                Err(RelayTicketError::NoSecret)
            );
        }
    }

    #[test]
    fn the_secret_delivery_stays_reachable_before_a_secret_exists() {
        // The one carve-out: the delivery cannot carry a ticket the drone
        // could check, and first-write-wins bounds it.
        assert_eq!(
            authorize(
                None,
                &request(RpcMethod::Post, BOOTSTRAP, b"{}", ""),
                ME,
                NOW
            ),
            Ok(None)
        );
        assert_eq!(
            authorize(
                None,
                &request(
                    RpcMethod::Post,
                    b"/api/relay/peer-secret?whatever=1",
                    b"",
                    ""
                ),
                ME,
                NOW
            ),
            Ok(None)
        );
    }

    #[test]
    fn the_carve_out_is_that_one_method_and_that_one_path() {
        for (method, path) in [
            (RpcMethod::Get, BOOTSTRAP),
            (RpcMethod::Put, BOOTSTRAP),
            (RpcMethod::Delete, BOOTSTRAP),
            (
                RpcMethod::Post,
                b"/api/relay/peer-secret/../config" as &[u8],
            ),
            (RpcMethod::Post, b"/api/relay/peer-secretx"),
            (RpcMethod::Post, b"/api/relay/peer-secre"),
        ] {
            assert_eq!(
                authorize(None, &request(method, path, b"", ""), ME, NOW),
                Err(RelayTicketError::NoSecret),
                "{method:?} {}",
                String::from_utf8_lossy(path)
            );
        }
    }

    #[test]
    fn a_ticket_bound_to_the_request_is_admitted() {
        let got = authorize(
            Some(SECRET),
            &ticketed(RpcMethod::Put, ORDINARY, b"{}"),
            ME,
            NOW,
        );
        assert!(matches!(got, Ok(Some(_))));
    }

    #[test]
    fn a_ticket_for_one_request_does_not_authorize_another() {
        // Every fleet member hears the uplink. A ticket minted for a status
        // read must not carry a command.
        let read = ticket(SECRET, ME, RpcMethod::Get, b"/api/status", b"", NOW);
        assert_eq!(
            authorize(
                Some(SECRET),
                &request(
                    RpcMethod::Post,
                    b"/api/command",
                    b"{\"cmd\":\"arm\"}",
                    &read
                ),
                ME,
                NOW
            ),
            Err(RelayTicketError::BindingMismatch)
        );
    }

    #[test]
    fn the_same_ticket_is_honoured_once() {
        let req = ticketed(RpcMethod::Get, b"/api/status", b"");
        let first = authorize(Some(SECRET), &req, ME, NOW).unwrap().unwrap();
        assert_eq!(first.spend(&SPENT_TICKETS, NOW), Ok(()));
        let second = authorize(Some(SECRET), &req, ME, NOW + 1).unwrap().unwrap();
        assert_eq!(
            second.spend(&SPENT_TICKETS, NOW + 1),
            Err(RelayTicketError::Replayed)
        );
    }

    #[test]
    fn a_skewed_clock_is_tolerated_within_bounds() {
        let req = |at: i64| {
            request(
                RpcMethod::Get,
                b"/api/status",
                b"",
                &ticket(SECRET, ME, RpcMethod::Get, b"/api/status", b"", at),
            )
        };
        assert!(authorize(Some(SECRET), &req(NOW + 200), ME, NOW).is_ok());
        assert_eq!(
            authorize(Some(SECRET), &req(NOW + 400), ME, NOW),
            Err(RelayTicketError::ClockSkew)
        );
    }

    #[test]
    fn a_drone_holding_a_secret_refuses_a_call_carrying_none() {
        // Once the credential exists, an unaccompanied relayed request no
        // longer inherits on-box authority -- including on the delivery
        // route, which a re-key attempt would have to use.
        assert!(authorize(
            Some(SECRET),
            &request(RpcMethod::Put, ORDINARY, b"", ""),
            ME,
            NOW
        )
        .is_err());
        assert!(authorize(
            Some(SECRET),
            &request(RpcMethod::Post, BOOTSTRAP, b"", ""),
            ME,
            NOW
        )
        .is_err());
    }

    #[test]
    fn a_ticket_minted_for_another_drone_is_refused() {
        let t = ticket(
            SECRET,
            "some-other-drone",
            RpcMethod::Put,
            ORDINARY,
            b"",
            NOW,
        );
        assert_eq!(
            authorize(
                Some(SECRET),
                &request(RpcMethod::Put, ORDINARY, b"", &t),
                ME,
                NOW
            ),
            Err(RelayTicketError::BindingMismatch)
        );
    }

    #[test]
    fn a_ticket_from_a_different_pair_secret_is_refused() {
        // A second ground station holding the shared fleet radio key still
        // cannot mint one this drone accepts.
        let other = "f".repeat(64);
        let t = ticket(&other, ME, RpcMethod::Put, ORDINARY, b"", NOW);
        assert_eq!(
            authorize(
                Some(SECRET),
                &request(RpcMethod::Put, ORDINARY, b"", &t),
                ME,
                NOW
            ),
            Err(RelayTicketError::BadSignature)
        );
    }

    #[test]
    fn an_expired_ticket_is_refused() {
        let t = ticket(SECRET, ME, RpcMethod::Put, ORDINARY, b"", NOW - 400);
        assert_eq!(
            authorize(
                Some(SECRET),
                &request(RpcMethod::Put, ORDINARY, b"", &t),
                ME,
                NOW
            ),
            Err(RelayTicketError::Expired)
        );
    }

    #[test]
    fn a_ticket_that_is_not_utf8_is_refused_rather_than_panicking() {
        let mut req = request(RpcMethod::Put, ORDINARY, b"", "");
        req.ticket = vec![0xFF, 0xFE, 0xFD];
        assert!(authorize(Some(SECRET), &req, ME, NOW).is_err());
    }
}
