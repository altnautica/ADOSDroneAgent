//! The captive portal end to end on loopback: real UDP DNS and TCP HTTP.

use std::net::{Ipv4Addr, SocketAddr, TcpListener as StdTcp, UdpSocket as StdUdp};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

/// A loopback address with a currently free port of each kind.
fn free_ports() -> (SocketAddr, SocketAddr) {
    let udp = StdUdp::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let tcp = StdTcp::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    (udp, tcp)
}

fn query(txid: u16, name: &str) -> Vec<u8> {
    let mut q = vec![0u8; 12];
    q[0..2].copy_from_slice(&txid.to_be_bytes());
    q[2..4].copy_from_slice(&0x0100u16.to_be_bytes());
    q[4..6].copy_from_slice(&1u16.to_be_bytes());
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    q
}

async fn ask(server: SocketAddr, txid: u16, name: &str) -> Vec<u8> {
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let q = query(txid, name);
    let mut buf = [0u8; 512];
    // The server task may still be binding: resend until it answers.
    for _ in 0..50 {
        client.send_to(&q, server).await.unwrap();
        if let Ok(Ok((n, _))) =
            tokio::time::timeout(Duration::from_millis(100), client.recv_from(&mut buf)).await
        {
            return buf[..n].to_vec();
        }
    }
    panic!("no DNS answer from {server}");
}

async fn http_get(server: SocketAddr, path: &str) -> String {
    for _ in 0..50 {
        if let Ok(mut stream) = TcpStream::connect(server).await {
            let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n");
            stream.write_all(req.as_bytes()).await.unwrap();
            let mut out = String::new();
            stream.read_to_string(&mut out).await.unwrap();
            return out;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no HTTP listener on {server}");
}

#[tokio::test]
async fn answers_captured_names_nxdomains_the_rest_and_redirects_http() {
    let (dns, http) = free_ports();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(ados_net::captive::run_on(dns, http, 8080, async {
        let _ = stop_rx.await;
    }));

    let answer = ask(dns, 0xA1B2, "captive.apple.com").await;
    assert_eq!(&answer[0..2], &[0xA1, 0xB2]);
    assert_eq!(&answer[2..4], &[0x81, 0x80], "QR/RD/RA, no error");
    assert_eq!(&answer[6..8], &[0, 1], "one answer");
    let rr = &answer[answer.len() - 16..];
    assert_eq!(&rr[0..2], &[0xC0, 0x0C]);
    assert_eq!(&rr[2..6], &[0, 1, 0, 1], "A / IN");
    assert_eq!(u32::from_be_bytes([rr[6], rr[7], rr[8], rr[9]]), 60);
    assert_eq!(&rr[10..12], &[0, 4]);
    assert_eq!(
        Ipv4Addr::new(rr[12], rr[13], rr[14], rr[15]),
        Ipv4Addr::new(192, 168, 4, 1)
    );

    let nx = ask(dns, 7, "example.org").await;
    assert_eq!(&nx[2..4], &[0x81, 0x83], "NXDOMAIN");
    assert_eq!(&nx[6..8], &[0, 0], "no answer records");

    let resp = http_get(http, "/generate_204").await;
    assert!(resp.starts_with("HTTP/1.0 302 "), "{resp}");
    assert!(
        resp.contains("\r\nLocation: http://192.168.4.1:8080/\r\n"),
        "{resp}"
    );

    stop_tx.send(()).unwrap();
    let code = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the portal stops on shutdown")
        .unwrap();
    assert_eq!(code, std::process::ExitCode::SUCCESS);
}

#[tokio::test]
async fn a_taken_dns_port_stands_the_portal_down() {
    let taken = StdUdp::bind("127.0.0.1:0").unwrap();
    let dns = taken.local_addr().unwrap();
    let (_, http) = free_ports();
    let code = tokio::time::timeout(
        Duration::from_secs(5),
        ados_net::captive::run_on(dns, http, 8080, std::future::pending()),
    )
    .await
    .expect("returns without serving");
    assert_eq!(code, std::process::ExitCode::SUCCESS);
}
