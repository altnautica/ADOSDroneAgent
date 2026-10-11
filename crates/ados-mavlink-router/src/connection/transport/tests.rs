use super::*;

/// How many simulated writes are in flight at once, and the most ever seen.
static ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static PEAK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A slow disk write that records overlap and keeps the last body written.
fn slow_write(path: &std::path::Path, body: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::Ordering;
    let now = ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
    PEAK.fetch_max(now, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(15));
    std::fs::write(path, body)?;
    ACTIVE.fetch_sub(1, Ordering::SeqCst);
    Ok(())
}

/// A burst of cache snapshots (a bulk parameter write) is written by one
/// writer at a time and ends on the newest snapshot, never an older one.
#[tokio::test]
async fn a_burst_of_cache_writes_is_serialised_and_ends_on_the_newest() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("params.json");
    let persister = ParamPersister::with_writer(slow_write);
    for i in 0..6u8 {
        persister.persist(path.clone(), vec![i]);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    for _ in 0..200 {
        let idle = !persister
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .writing;
        if idle {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(PEAK.load(Ordering::SeqCst), 1, "writes must never overlap");
    assert_eq!(std::fs::read(&path).unwrap(), vec![5u8]);
}

#[test]
fn parse_net_spec_detects_tcp_and_udp() {
    assert_eq!(
        parse_net_spec("tcp:127.0.0.1:5760"),
        Some(NetSpec::Tcp("127.0.0.1:5760".to_string()))
    );
    assert_eq!(
        parse_net_spec("udp:127.0.0.1:14550"),
        Some(NetSpec::Udp("127.0.0.1:14550".to_string()))
    );
    // udpout/udpin map to the same connect path.
    assert_eq!(
        parse_net_spec("udpout:10.0.0.5:14555"),
        Some(NetSpec::Udp("10.0.0.5:14555".to_string()))
    );
    // A bare host defaults to the conventional MAVLink UDP port.
    assert_eq!(
        parse_net_spec("udp:localhost"),
        Some(NetSpec::Udp("localhost:14550".to_string()))
    );
}

/// A USB serial port whose device name does not match a known prefix. The
/// candidate gate must keep it on the strength of its USB type alone so a
/// non-standard-named FC is still auto-discovered.
fn usb_port_type() -> SerialPortType {
    SerialPortType::UsbPort(tokio_serial::UsbPortInfo {
        vid: 0x1209,
        pid: 0x5741,
        serial_number: None,
        manufacturer: None,
        product: None,
    })
}

#[test]
fn candidate_gate_keeps_typed_usb_port_with_non_prefix_name() {
    // A USB port enumerating under a name no SERIAL_PREFIX matches.
    assert!(
        is_candidate_port(&usb_port_type(), "/dev/ttyGS0"),
        "a typed USB port must survive regardless of its device name"
    );
    assert!(
        is_candidate_port(&usb_port_type(), "/dev/ttymxc3"),
        "a vendor-named USB tty must survive"
    );
}

#[test]
fn candidate_gate_keeps_prefix_named_non_usb_port() {
    // A non-USB port whose name matches a prefix (e.g. an on-board UART).
    assert!(is_candidate_port(&SerialPortType::Unknown, "/dev/ttyAMA0"));
    assert!(is_candidate_port(&SerialPortType::PciPort, "/dev/ttyS0"));
}

#[test]
fn candidate_gate_rejects_non_usb_unprefixed_port() {
    // Neither USB nor a known prefix: a virtual/Bluetooth port is skipped.
    assert!(!is_candidate_port(
        &SerialPortType::BluetoothPort,
        "/dev/rfcomm0"
    ));
    assert!(!is_candidate_port(
        &SerialPortType::Unknown,
        "/dev/something-else"
    ));
}

#[test]
fn same_device_matches_literal_and_resolved_paths() {
    // Literal equality needs no filesystem.
    assert!(same_device("/dev/ttyUSB0", "/dev/ttyUSB0"));
    // Distinct unresolvable paths never compare equal.
    assert!(!same_device("/dev/ttyUSB0", "/dev/ttyUSB1"));
    assert!(!same_device("/dev/ttyUSB0", ""));
}

#[cfg(unix)]
#[test]
fn same_device_resolves_a_symlink_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("ttyUSB0");
    std::fs::write(&real, b"").unwrap();
    let link = dir.path().join("by-id-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(same_device(real.to_str().unwrap(), link.to_str().unwrap()));
    // A symlink to a DIFFERENT node stays unequal.
    let other = dir.path().join("ttyUSB1");
    std::fs::write(&other, b"").unwrap();
    assert!(!same_device(
        other.to_str().unwrap(),
        link.to_str().unwrap()
    ));
}

#[tokio::test]
async fn udp_listen_locks_to_the_first_peer_and_keeps_the_first_datagram() {
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender.send_to(b"first", addr).await.unwrap();

    let (mut rd, mut wr, label, baud) = tokio::time::timeout(
        Duration::from_secs(5),
        udp_listen_lock_first_peer(listener, Duration::from_secs(5)),
    )
    .await
    .expect("handshake must not hang")
    .expect("a queued datagram must resolve the peer");
    assert_eq!(label, format!("udpin:{addr}"));
    assert_eq!(baud, 0, "a network transport reports no baud");

    // The peeked datagram was preserved: the read half yields it whole.
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(5), rd.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], b"first");

    // The write half reaches the locked peer (the module's return path).
    wr.write_all(b"reply").await.unwrap();
    wr.flush().await.unwrap();
    let mut rbuf = [0u8; 16];
    let (n, from) = tokio::time::timeout(Duration::from_secs(5), sender.recv_from(&mut rbuf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&rbuf[..n], b"reply");
    assert_eq!(from, addr);
}

#[tokio::test]
async fn udp_listen_reports_none_when_nothing_talks() {
    // A silent window is an unresolved open, not a hang: the caller's
    // backoff owns the retry.
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    assert!(
        udp_listen_lock_first_peer(listener, Duration::from_millis(50))
            .await
            .is_none()
    );
}

#[test]
fn parse_net_spec_rejects_serial_paths() {
    assert_eq!(parse_net_spec("/dev/ttyACM0"), None);
    assert_eq!(parse_net_spec("/dev/ttyS0"), None);
    assert_eq!(parse_net_spec(""), None);
    // An unknown scheme is not a network transport.
    assert_eq!(parse_net_spec("serial:/dev/ttyUSB0"), None);
}
