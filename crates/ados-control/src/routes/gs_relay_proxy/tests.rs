use super::{path_is_safe, raw_forward_path, relayed_response};
use ados_protocol::aux_rpc_proxy::RpcResponseOwned;

fn relayed(status: u16, headers: &[(&str, &str)], body: &[u8]) -> axum::response::Response {
    relayed_response(RpcResponseOwned {
        status,
        body: body.to_vec(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    })
}

#[test]
fn a_relayed_bodys_content_type_survives_the_proxy() {
    let resp = relayed(
        200,
        &[("content-type", "application/octet-stream")],
        b"\x00\x01\x02",
    );
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap()),
        Some("application/octet-stream"),
        "a relayed body must arrive with the type the drone gave it"
    );
}

#[test]
fn a_whep_location_and_a_download_filename_cross_too() {
    // The two headers whose absence silently broke a whole feature: WHEP
    // session teardown routes through `Location`, and an artifact download
    // is named by `Content-Disposition`.
    let resp = relayed(
        201,
        &[
            ("location", "/whep/session/abc123"),
            (
                "content-disposition",
                "attachment; filename=\"flight.ndjson\"",
            ),
        ],
        b"",
    );
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    assert_eq!(
        resp.headers().get("location").unwrap(),
        "/whep/session/abc123"
    );
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        "attachment; filename=\"flight.ndjson\""
    );
}

#[test]
fn a_header_the_radio_damaged_is_dropped_and_the_body_still_arrives() {
    // A corrupt header must never cost the answer. An embedded newline is
    // the case that matters: an HTTP layer that accepted it would be
    // response-splitting.
    let resp = relayed(
        200,
        &[("content-type", "text/plain\r\nX-Injected: 1")],
        b"still here",
    );
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert!(resp.headers().get("x-injected").is_none());
    assert!(resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .is_none());
}

#[test]
fn a_drone_that_sent_no_headers_still_yields_its_status_and_body() {
    // The compatibility case: an agent older than the header block reports
    // an empty list, and the route must behave exactly as it always did.
    let resp = relayed(404, &[], br#"{"detail":"Not Found"}"#);
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    assert!(resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .is_none());
}

#[test]
fn an_ordinary_path_with_a_query_string_is_forwarded() {
    assert!(path_is_safe("/api/version"));
    assert!(path_is_safe("/api/logs?limit=5&level=warn"));
    assert!(path_is_safe("/api/status/full"));
}

#[test]
fn a_decoded_crlf_is_rejected_before_it_reaches_the_radio() {
    // `%0d%0aX-Injected:%201` after percent-decoding.
    assert!(!path_is_safe("/api/version\r\nX-Injected: 1"));
    assert!(!path_is_safe("/api/version\nX-Injected: 1"));
    assert!(!path_is_safe("/api/version\rX-Injected: 1"));
}

#[test]
fn other_control_characters_are_rejected_too() {
    assert!(!path_is_safe("/api/\0version"));
    assert!(!path_is_safe("/api/\tversion"));
    assert!(!path_is_safe("/api/version\x7f"));
    // A bare space would split the drone's request line.
    assert!(!path_is_safe("/api/v1/network/configured/Home Net"));
}

#[test]
fn the_forwarded_path_stays_percent_encoded() {
    // A saved network named "Home Net" (or with a `?` in it) must reach the
    // drone as the same encoded path the operator's client sent.
    let raw =
        "/api/v1/ground-station/relay-proxy/0a1b2c3d4e5f/api/v1/network/configured/Home%20Net%3F";
    let path = raw_forward_path(raw).unwrap();
    assert_eq!(path, "/api/v1/network/configured/Home%20Net%3F");
    assert!(path_is_safe(path));
    // An encoded slash stays one path segment.
    assert_eq!(
        raw_forward_path("/api/v1/ground-station/relay-proxy/d1/api/x/a%2Fb"),
        Some("/api/x/a%2Fb")
    );
    assert_eq!(
        raw_forward_path("/api/v1/ground-station/relay-proxy/d1/"),
        None
    );
    assert_eq!(
        raw_forward_path("/api/v1/ground-station/relay-proxy/d1"),
        None
    );
}

fn registered(device_id: &str, secret: Option<&str>) -> Vec<ados_groundlink::FleetSlot> {
    vec![ados_groundlink::FleetSlot {
        slot: 1,
        device_id: device_id.to_string(),
        paired_at_ms: 0,
        relay_secret: secret.map(str::to_string),
    }]
}

#[tokio::test]
async fn a_relayed_call_carries_a_ticket_the_drone_admits() {
    use crate::routes::gs_fleet_slot::test_drone::{self, SECRET};
    use ados_protocol::aux_rpc::RpcMethod;

    const DRONE: &str = "ados-relay-01";
    let drone = test_drone::spawn(DRONE, Some(SECRET)).await;

    let resp = super::forward(
        &drone.proxy,
        &registered(DRONE, Some(SECRET)),
        DRONE,
        RpcMethod::Get,
        b"/api/version",
        b"",
        b"",
    )
    .await
    .expect("the drone answers");
    assert_eq!(resp.status, 200, "the drone must admit the relayed call");
    assert_eq!(drone.seen.lock().last().unwrap().path, b"/api/version");

    // A body and a PATCH cross too, bound into the ticket.
    let resp = super::forward(
        &drone.proxy,
        &registered(DRONE, Some(SECRET)),
        DRONE,
        RpcMethod::Patch,
        b"/api/plugins/com.example.tool/x/state",
        b"application/json",
        br#"{"on":true}"#,
    )
    .await
    .expect("the drone answers");
    assert_eq!(resp.status, 200);
}

#[tokio::test]
async fn a_peer_with_no_secret_is_refused_without_spending_airtime() {
    use crate::routes::gs_fleet_slot::test_drone::{self, SECRET};
    use ados_protocol::aux_rpc::RpcMethod;

    const DRONE: &str = "ados-relay-02";
    let drone = test_drone::spawn(DRONE, Some(SECRET)).await;
    let got = super::forward(
        &drone.proxy,
        &registered(DRONE, None),
        DRONE,
        RpcMethod::Get,
        b"/api/version",
        b"",
        b"",
    )
    .await;
    assert!(matches!(got, Err(super::Refusal::Ticket("no_secret"))));
    assert!(drone.seen.lock().is_empty(), "nothing crossed the radio");
}

#[tokio::test]
async fn an_unregistered_peer_is_not_linked_rather_than_a_radio_round_trip() {
    use crate::routes::gs_fleet_slot::test_drone::{self, SECRET};
    use ados_protocol::aux_rpc::RpcMethod;

    let drone = test_drone::spawn("ados-relay-03", Some(SECRET)).await;
    let got = super::forward(
        &drone.proxy,
        &registered("ados-relay-03", Some(SECRET)),
        "ados-relay-typo",
        RpcMethod::Get,
        b"/api/version",
        b"",
        b"",
    )
    .await;
    assert!(matches!(got, Err(super::Refusal::NotLinked)));
    assert!(drone.seen.lock().is_empty());
}
