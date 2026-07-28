//! `wfb_tx` live management-command client (the wfb-ng 24.08 `wfb_tx_cmd` wire).
//!
//! wfb-ng 24.08 added a management socket to `wfb_tx`: bound with `-C <port>`
//! (`vendor/wfb-ng/src/tx.cpp:1805-1807` getopt, `open_control_fd` at
//! `tx.cpp:1406-1425`, served inline in the transmit select loop at
//! `tx.cpp:833-978`). It applies a new Reed-Solomon ratio or a new radiotap
//! header **to the running transmitter**, replacing the kill-and-respawn path
//! that costs a 1-2 s video blackout per tier change.
//!
//! # Wire protocol
//!
//! UDP on loopback. `wfb_tx` binds its control socket to `127.0.0.1` only
//! (`tx.cpp:1408`, "bind to 127.0.0.1 for security reasons") and answers with
//! `sendto` back to the request's source address, so the client MUST send from a
//! loopback source. One request datagram, one response datagram.
//!
//! Request (`cmd_req_t`, `vendor/wfb-ng/src/tx_cmd.h`, `__attribute__((packed))`):
//!
//! ```text
//! byte 0..4   req_id  (u32, opaque; the server echoes the 4 bytes verbatim)
//! byte 4      cmd_id  (u8)
//! byte 5..    command body (the packed union arm selected by cmd_id)
//! ```
//!
//! `offsetof(cmd_req_t, u) == 5`, so the body starts at byte 5 and the datagram
//! length selects the arm. The server length-checks each arm exactly
//! (`tx.cpp:855`, `:891`, `:933`, `:951`) and replies `EINVAL` on a mismatch:
//!
//! | cmd_id | command      | body                                                                  | request len |
//! |--------|--------------|-----------------------------------------------------------------------|-------------|
//! | 1      | `SET_FEC`    | `k:u8, n:u8`                                                          | 7           |
//! | 2      | `SET_RADIO`  | `stbc:u8, ldpc:u8, short_gi:u8, bandwidth:u8, mcs:u8, vht:u8, nss:u8`  | 12          |
//! | 3      | `GET_FEC`    | none                                                                  | 5           |
//! | 4      | `GET_RADIO`  | none                                                                  | 5           |
//!
//! The three `bool` fields in `cmd_set_radio` are C `_Bool` inside a packed
//! struct — one byte each, written as 0 or 1.
//!
//! Response (`cmd_resp_t`):
//!
//! ```text
//! byte 0..4   req_id  (echo of the request's 4 bytes, `tx.cpp:848`)
//! byte 4..8   rc      (u32 BE — the server writes `htonl(errno)`, `tx.cpp:857`;
//!                      the reference client reads `ntohl`, `tx_cmd.c:107`)
//! byte 8..    response body: 0 bytes for the SET commands, 2 for GET_FEC,
//!                      7 for GET_RADIO
//! ```
//!
//! `offsetof(cmd_resp_t, u) == 8`. [`decode_response`] applies the reference
//! client's validation in its exact order (`tx_cmd.c:98-117`): short datagram →
//! invalid, `req_id` mismatch → invalid, `rc != 0` → failed with that errno,
//! then an exact total-length check.
//!
//! Every offset above was verified by compiling `tx_cmd.h` and dumping the packed
//! structs, not by reading alone; the encoder's byte output is identical to the
//! reference client's for `set_fec`, `set_radio` and both queries.
//!
//! # What this client is allowed to change
//!
//! Only the MCS index and the FEC pair. `set_radio` carries the **whole**
//! radiotap trio, so a caller that only wants a new MCS must still send the live
//! bandwidth / GI / STBC / LDPC values or the running transmitter would be
//! silently retuned. [`RadioSettings::with_mcs`] is the only constructor the
//! adaptive path uses: it pins `bandwidth = 20` and passes everything else
//! through unchanged. Channel width is deliberately not a knob — the vendored
//! `rtl8812eu` has no narrowband symbol compiled in (10 MHz needs a driver
//! rebuild) and 40 MHz has open upstream defects on this chipset family.

mod wire;

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::net::UdpSocket;

pub use wire::{
    decode_response, encode_query, encode_set_fec, encode_set_radio, RadioSettings, RespBody,
    TxCmdError, CMD_GET_FEC, CMD_GET_RADIO, CMD_SET_FEC, CMD_SET_RADIO, FEC_BODY_LEN,
    PINNED_BANDWIDTH_MHZ, RADIO_BODY_LEN, REQ_HEADER_LEN, RESP_HEADER_LEN,
};

/// Base of the per-instance `wfb_tx` control-port range: a transmitter on
/// `radio_port` R binds `-C TX_CMD_PORT_BASE + R`. One `wfb_tx` per radio port
/// per node, so the range is collision-free by construction: drone data 8000,
/// drone/ground control 8001, drone aux-down 8002, ground aux-up 8003.
pub const TX_CMD_PORT_BASE: u16 = 8000;

/// The control port for a `wfb_tx` instance on `radio_port`.
pub const fn control_port(radio_port: u8) -> u16 {
    TX_CMD_PORT_BASE + radio_port as u16
}

/// The reference client's fixed 3 s deadline (`vendor/wfb-ng/src/tx_cmd.c:32`,
/// enforced there with `alarm(COMMAND_TIMEOUT)`).
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

/// Request-id source. The reference client uses `htonl(rand())`; the id only has
/// to distinguish a fresh answer from a stale datagram on a socket this client
/// binds per command, so a monotonic counter is strictly stronger than random.
fn next_req_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A `wfb_tx` management-socket client, addressed by control port.
///
/// Cheap and stateless: each command binds its own ephemeral loopback socket, so
/// a client is safe to hold across a `wfb_tx` respawn and two concurrent
/// commands can never see each other's replies.
#[derive(Debug, Clone, Copy)]
pub struct TxCmdClient {
    port: u16,
    timeout: Duration,
}

impl TxCmdClient {
    /// Client for the `wfb_tx` instance whose `-C` port is `port`.
    pub fn new(port: u16) -> Self {
        Self {
            port,
            timeout: COMMAND_TIMEOUT,
        }
    }

    /// Client for the `wfb_tx` instance serving `radio_port`.
    pub fn for_radio_port(radio_port: u8) -> Self {
        Self::new(control_port(radio_port))
    }

    /// Override the response deadline (the tests drive this well under 3 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Apply a Reed-Solomon `(k, n)` ratio to the running transmitter. `wfb_tx`
    /// restarts its FEC session in place (`tx.cpp:884-885`) — the stream keeps
    /// running, unlike a respawn.
    pub async fn set_fec(&self, fec_k: u8, fec_n: u8) -> Result<(), TxCmdError> {
        let id = next_req_id();
        self.round_trip(CMD_SET_FEC, id, encode_set_fec(id, fec_k, fec_n))
            .await
            .map(|_| ())
    }

    /// Replace the injected radiotap header on the running transmitter. Send the
    /// whole live trio, not a delta — see [`RadioSettings::with_mcs`].
    pub async fn set_radio(&self, s: &RadioSettings) -> Result<(), TxCmdError> {
        let id = next_req_id();
        self.round_trip(CMD_SET_RADIO, id, encode_set_radio(id, s))
            .await
            .map(|_| ())
    }

    /// Read back the live Reed-Solomon ratio.
    pub async fn get_fec(&self) -> Result<(u8, u8), TxCmdError> {
        let id = next_req_id();
        match self
            .round_trip(CMD_GET_FEC, id, encode_query(id, CMD_GET_FEC))
            .await?
        {
            RespBody::Fec { fec_k, fec_n } => Ok((fec_k, fec_n)),
            _ => Err(TxCmdError::BadLength {
                got: RESP_HEADER_LEN,
                want: RESP_HEADER_LEN + FEC_BODY_LEN,
            }),
        }
    }

    /// Read back what the transmitter is actually radiating. The honest readback
    /// the adaptive ladder logs after a step: the driver can ignore a rung.
    pub async fn get_radio(&self) -> Result<RadioSettings, TxCmdError> {
        let id = next_req_id();
        match self
            .round_trip(CMD_GET_RADIO, id, encode_query(id, CMD_GET_RADIO))
            .await?
        {
            RespBody::Radio(s) => Ok(s),
            _ => Err(TxCmdError::BadLength {
                got: RESP_HEADER_LEN,
                want: RESP_HEADER_LEN + RADIO_BODY_LEN,
            }),
        }
    }

    /// One request datagram out, one response datagram in, on a fresh ephemeral
    /// loopback socket. `connect` is what makes the ICMP port-unreachable from a
    /// dead `wfb_tx` surface as ECONNREFUSED instead of hanging until timeout.
    async fn round_trip(
        &self,
        cmd_id: u8,
        req_id: u32,
        req: Vec<u8>,
    ) -> Result<RespBody, TxCmdError> {
        let sock = UdpSocket::bind(("127.0.0.1", 0)).await?;
        sock.connect(("127.0.0.1", self.port)).await?;
        sock.send(&req).await?;
        // `cmd_resp_t` is 15 bytes at its largest; read into a full MTU so an
        // over-long datagram is seen as over-long rather than silently truncated
        // to the exact expected length.
        let mut buf = [0u8; 256];
        let n = match tokio::time::timeout(self.timeout, sock.recv(&mut buf)).await {
            Ok(r) => r?,
            Err(_) => return Err(TxCmdError::Timeout),
        };
        decode_response(cmd_id, req_id, &buf[..n])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `wfb_tx` instance gets its own control port, derived from the radio
    /// port it serves. Two planes sharing a port would let a data-plane FEC
    /// change land on the control plane.
    #[test]
    fn control_ports_are_unique_per_radio_port() {
        assert_eq!(control_port(0), 8000);
        assert_eq!(control_port(1), 8001);
        assert_eq!(control_port(2), 8002);
        assert_eq!(control_port(3), 8003);
        let ports: Vec<u16> = (0..=3).map(control_port).collect();
        let mut uniq = ports.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), ports.len());
    }

    /// Stand in for `wfb_tx`'s control socket, applying the real server's
    /// validation (`tx.cpp:833-978`) rather than blindly acknowledging:
    ///
    /// - a datagram shorter than the 5-byte header is dropped (`tx.cpp:846`);
    /// - each command's body length is checked EXACTLY, and a mismatch answers a
    ///   bare header with `rc = htonl(EINVAL)` (`tx.cpp:855-860`, `:891-896`,
    ///   `:933-938`, `:951-956`);
    /// - an unknown `cmd_id` answers `ENOTSUP` (`tx.cpp:974-975`);
    /// - `req_id` is echoed verbatim, never byte-swapped (`tx.cpp:848`).
    ///
    /// This is what makes the round-trip test a conformance check: a client that
    /// sent a 13-byte `set_radio` would be rejected here exactly as the real
    /// transmitter rejects it, instead of being waved through by a lenient stub.
    async fn fake_wfb_tx(sock: UdpSocket, replies: usize) -> tokio::task::JoinHandle<Vec<Vec<u8>>> {
        const EINVAL: u32 = 22;
        const ENOTSUP: u32 = 45;
        tokio::spawn(async move {
            let mut seen = Vec::new();
            let mut buf = [0u8; 256];
            for _ in 0..replies {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    break;
                };
                if n < REQ_HEADER_LEN {
                    continue; // tx.cpp:846 — silently ignored, no reply at all.
                }
                let req = buf[..n].to_vec();
                let cmd_id = req[4];
                let want_body = match cmd_id {
                    CMD_SET_FEC => Some(FEC_BODY_LEN),
                    CMD_SET_RADIO => Some(RADIO_BODY_LEN),
                    CMD_GET_FEC | CMD_GET_RADIO => Some(0),
                    _ => None,
                };
                let mut out = Vec::new();
                out.extend_from_slice(&req[..4]); // echo req_id verbatim
                match want_body {
                    None => out.extend_from_slice(&ENOTSUP.to_be_bytes()),
                    Some(want) if n != REQ_HEADER_LEN + want => {
                        out.extend_from_slice(&EINVAL.to_be_bytes());
                    }
                    Some(_) => {
                        out.extend_from_slice(&0u32.to_be_bytes());
                        match cmd_id {
                            CMD_GET_FEC => out.extend_from_slice(&[8, 12]),
                            CMD_GET_RADIO => out.extend_from_slice(&[0, 0, 0, 20, 5, 0, 1]),
                            _ => {}
                        }
                    }
                }
                let _ = sock.send_to(&out, from).await;
                seen.push(req);
            }
            seen
        })
    }

    /// End-to-end over a real loopback UDP socket against a server that enforces
    /// `tx.cpp`'s exact length rules: the client's datagrams are byte-exact and it
    /// accepts the server's replies.
    #[tokio::test]
    async fn client_round_trips_against_a_fake_wfb_tx() {
        let sock = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let server = fake_wfb_tx(sock, 4).await;

        let client = TxCmdClient::new(port).with_timeout(Duration::from_secs(2));
        client.set_fec(8, 16).await.expect("set_fec");
        client
            .set_radio(&RadioSettings::with_mcs(5))
            .await
            .expect("set_radio");
        assert_eq!(client.get_fec().await.expect("get_fec"), (8, 12));
        let radio = client.get_radio().await.expect("get_radio");
        assert_eq!(radio.mcs_index, 5);
        assert_eq!(radio.bandwidth, 20);

        let seen = server.await.unwrap();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0][4..], [CMD_SET_FEC, 8, 16]);
        assert_eq!(seen[1][4..], [CMD_SET_RADIO, 0, 0, 0, 20, 5, 0, 1]);
        assert_eq!(seen[2].len(), REQ_HEADER_LEN);
        assert_eq!(seen[3].len(), REQ_HEADER_LEN);
    }

    /// Guard on the guard: prove the fake actually enforces the length rule, so
    /// the round-trip above is a conformance check and not a lenient echo. A
    /// hand-rolled over-long `set_radio` (one stray trailing byte, 13 instead of
    /// 12) must come back EINVAL, exactly as `tx.cpp:891-896` answers it.
    #[tokio::test]
    async fn a_wrong_length_request_is_rejected_with_einval() {
        let sock = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let server = fake_wfb_tx(sock, 1).await;

        let mut bad = encode_set_radio(11, &RadioSettings::with_mcs(3));
        bad.push(0xFF); // 13 bytes — one too many.
        let client = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        client.connect(("127.0.0.1", port)).await.unwrap();
        client.send(&bad).await.unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf))
            .await
            .expect("answered")
            .expect("recv");

        // EINVAL (22), decoded big-endian, on a bare header.
        assert_eq!(n, RESP_HEADER_LEN);
        assert!(
            matches!(
                decode_response(CMD_SET_RADIO, 11, &buf[..n]).unwrap_err(),
                TxCmdError::Failed(22)
            ),
            "the fake must reject a wrong-length set_radio"
        );
        server.await.unwrap();
    }

    /// No `wfb_tx` on the port: the client must report `Unreachable` (so the
    /// caller falls back to a respawn) rather than blocking for the full 3 s.
    #[tokio::test]
    async fn missing_wfb_tx_reports_unreachable() {
        // Bind then drop, so the port is near-certainly free but was ours.
        let probe = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let client = TxCmdClient::new(port).with_timeout(Duration::from_millis(300));
        let err = client.set_fec(8, 12).await.expect_err("must not succeed");
        assert!(
            matches!(err, TxCmdError::Unreachable | TxCmdError::Timeout),
            "expected Unreachable/Timeout, got {err:?}"
        );
    }

    /// A `wfb_tx` that binds the port but never answers must time out, not hang.
    #[tokio::test]
    async fn silent_wfb_tx_times_out() {
        let sock = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let client = TxCmdClient::new(port).with_timeout(Duration::from_millis(200));
        let err = client
            .set_radio(&RadioSettings::with_mcs(3))
            .await
            .expect_err("must not succeed");
        assert!(
            matches!(err, TxCmdError::Timeout),
            "expected Timeout, got {err:?}"
        );
        drop(sock);
    }
}
