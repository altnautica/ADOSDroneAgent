use super::*;

fn query(txid: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut q = vec![0u8; 12];
    q[0..2].copy_from_slice(&txid.to_be_bytes());
    q[2..4].copy_from_slice(&0x0100u16.to_be_bytes()); // RD
    q[4..6].copy_from_slice(&1u16.to_be_bytes());
    q.extend(encode_name(name));
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes());
    q
}

#[test]
fn parses_a_question_lowercased() {
    let q = parse_question(&query(0xBEEF, "Captive.Apple.COM", 1)).unwrap();
    assert_eq!(
        q,
        Question {
            txid: 0xBEEF,
            qname: "captive.apple.com".into(),
            qtype: 1,
            qclass: 1
        }
    );
}

#[test]
fn malformed_queries_are_dropped() {
    assert_eq!(parse_question(&[0u8; 11]), None);
    // No question.
    let mut none = query(1, "a.b", 1);
    none[4..6].copy_from_slice(&0u16.to_be_bytes());
    assert_eq!(parse_question(&none), None);
    // A compression pointer in the question name.
    let mut ptr = vec![0u8; 12];
    ptr[4..6].copy_from_slice(&1u16.to_be_bytes());
    ptr.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1]);
    assert_eq!(parse_question(&ptr), None);
    // Truncated type/class.
    let full = query(1, "a.b", 1);
    assert_eq!(parse_question(&full[..full.len() - 2]), None);
    assert_eq!(dns_reply(&[0u8; 5]), None);
}

#[test]
fn answer_bytes_are_one_a_record_with_ttl_60() {
    let reply = build_answer(0x1234, "captive.apple.com", AP_GATEWAY_IP);
    let mut expected = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
    expected.extend(encode_name("captive.apple.com"));
    expected.extend_from_slice(&[0, 1, 0, 1, 0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
    expected.extend_from_slice(&[192, 168, 4, 1]);
    assert_eq!(reply, expected);
}

#[test]
fn nxdomain_echoes_the_question() {
    let reply = build_nxdomain(7, "example.org", 28, 1);
    let mut expected = vec![0, 7, 0x81, 0x83, 0, 1, 0, 0, 0, 0, 0, 0];
    expected.extend(encode_name("example.org"));
    expected.extend_from_slice(&[0, 28, 0, 1]);
    assert_eq!(reply, expected);
}

#[test]
fn only_a_queries_for_captured_hosts_are_answered() {
    let answered = dns_reply(&query(1, "connectivitycheck.gstatic.com", 1)).unwrap();
    assert_eq!(answered[3] & 0x0F, 0);
    assert_eq!(&answered[answered.len() - 4..], &[192, 168, 4, 1]);
    // AAAA for a captured host is NXDOMAIN, as is any other name.
    for (name, qtype) in [("captive.apple.com", 28), ("example.org", 1)] {
        let reply = dns_reply(&query(1, name, qtype)).unwrap();
        assert_eq!(reply[3] & 0x0F, 3, "{name} {qtype}");
    }
}

#[test]
fn every_get_and_head_redirects_to_the_setup_webapp() {
    for path in [
        "/generate_204",
        "/hotspot-detect.html",
        "/connecttest.txt",
        "/",
        "/anything",
    ] {
        let resp = String::from_utf8(http_response("GET", path, 8080)).unwrap();
        // A 204 on /generate_204 tells Android the network is validated and
        // suppresses the sign-in sheet; the portal must never answer it.
        assert!(resp.starts_with("HTTP/1.0 302 "), "{path}: {resp}");
        assert!(resp.contains("\r\nLocation: http://192.168.4.1:8080/\r\n"));
        assert!(resp.ends_with("Open ADOS setup</a>\n"));
    }
    let head = String::from_utf8(http_response("HEAD", "/", 8080)).unwrap();
    assert!(head.starts_with("HTTP/1.0 302 "));
    assert!(head.ends_with("\r\n\r\n"), "HEAD carries no body");
    assert!(http_response("POST", "/", 8080).starts_with(b"HTTP/1.0 501 "));
}

#[test]
fn the_redirect_target_is_never_the_portal_itself() {
    let url = setup_url(8080);
    assert_eq!(url, "http://192.168.4.1:8080/");
    assert!(!matches!(
        url.as_str(),
        "http://192.168.4.1/" | "http://192.168.4.1:80/"
    ));
}

#[test]
fn stands_down_when_setup_is_complete_or_dnsmasq_owns_dns() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("setup-complete");
    assert_eq!(stand_down_reason(&sentinel, false), None);
    assert!(stand_down_reason(&sentinel, true).is_some());
    std::fs::write(&sentinel, "1\n").unwrap();
    assert_eq!(
        stand_down_reason(&sentinel, false),
        Some("setup already complete")
    );
}

#[test]
fn api_port_comes_from_config_with_the_shipped_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    // Absent file: the schema default.
    assert_eq!(api_port(&cfg), 8080);
    std::fs::write(&cfg, "api:\n  rest:\n    port: 9090\n").unwrap();
    assert_eq!(api_port(&cfg), 9090);
    std::fs::write(&cfg, "api: [unparseable\n").unwrap();
    assert_eq!(api_port(&cfg), DEFAULT_API_PORT);
}
