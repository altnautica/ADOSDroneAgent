//! The `wfb_tx_cmd` wire codec — pure, no I/O.
//!
//! Every constant and offset here is taken from the vendored
//! `vendor/wfb-ng/src/tx_cmd.h` packed structs and the server's own length checks
//! in `vendor/wfb-ng/src/tx.cpp:833-978`. The full protocol narrative lives in
//! the parent module's docs; this file is the byte layout and nothing else.

use std::fmt;

/// `wfb_tx` command ids (`vendor/wfb-ng/src/tx_cmd.h:4-7`).
pub const CMD_SET_FEC: u8 = 1;
pub const CMD_SET_RADIO: u8 = 2;
pub const CMD_GET_FEC: u8 = 3;
pub const CMD_GET_RADIO: u8 = 4;

/// `offsetof(cmd_req_t, u)` — `req_id` (4) + `cmd_id` (1).
pub const REQ_HEADER_LEN: usize = 5;
/// `offsetof(cmd_resp_t, u)` — `req_id` (4) + `rc` (4).
pub const RESP_HEADER_LEN: usize = 8;
/// `sizeof(cmd_set_fec)` / `sizeof(cmd_get_fec)`.
pub const FEC_BODY_LEN: usize = 2;
/// `sizeof(cmd_set_radio)` / `sizeof(cmd_get_radio)`.
pub const RADIO_BODY_LEN: usize = 7;

/// The channel width every ADOS `wfb_tx` runs at, in MHz. Pinned: `wfb_tx`
/// accepts only 10, 20 or 40 (`vendor/wfb-ng/src/tx.cpp:1119-1130`), 10 needs a
/// driver rebuild this tree does not have, and 40 has open upstream defects on
/// the RTL8812xU family. Every `set_radio` this crate sends carries this value.
pub const PINNED_BANDWIDTH_MHZ: u8 = 20;

/// The full radiotap trio a `set_radio` command replaces. `wfb_tx` rebuilds its
/// injected radiotap header from all seven fields at once (`tx.cpp:899-915`), so
/// there is no partial update: every field must carry the value that should be
/// live afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioSettings {
    pub stbc: u8,
    pub ldpc: bool,
    pub short_gi: bool,
    pub bandwidth: u8,
    pub mcs_index: u8,
    pub vht_mode: bool,
    pub vht_nss: u8,
}

impl Default for RadioSettings {
    /// The `wfb_tx_cmd set_radio` defaults (`tx_cmd.c:158-164`), which are also
    /// the values ADOS spawns `wfb_tx` with — it passes only `-B 20 -M <mcs>` and
    /// lets `wfb_tx`'s own getopt defaults (`tx.cpp:1679-1691`) stand for the
    /// rest: 20 MHz, long GI, no STBC, no LDPC, HT (not VHT), one spatial stream.
    fn default() -> Self {
        Self {
            stbc: 0,
            ldpc: false,
            short_gi: false,
            bandwidth: PINNED_BANDWIDTH_MHZ,
            mcs_index: 1,
            vht_mode: false,
            vht_nss: 1,
        }
    }
}

impl RadioSettings {
    /// The settings for an MCS-only change: `mcs` over the pinned 20 MHz width
    /// and the spawn-time GI / STBC / LDPC / VHT values. This is the only
    /// constructor the adaptive ladder uses, so an MCS step can never retune the
    /// channel width even though `set_radio` replaces the whole header.
    pub fn with_mcs(mcs_index: u8) -> Self {
        Self {
            mcs_index,
            ..Self::default()
        }
    }
}

/// Why a management command did not take effect.
#[derive(Debug)]
pub enum TxCmdError {
    /// Nothing is listening on the control port — no `wfb_tx` with `-C`, or it
    /// died. The caller falls back to a respawn.
    Unreachable,
    /// No response within [`super::COMMAND_TIMEOUT`].
    Timeout,
    /// Socket-level failure.
    Io(std::io::Error),
    /// Response shorter than the header, or not the exact length the command's
    /// response body requires.
    BadLength { got: usize, want: usize },
    /// Response carried another request's id — a stale datagram.
    ReqIdMismatch { sent: u32, got: u32 },
    /// `wfb_tx` rejected the command; the payload is the errno it returned
    /// (`EINVAL` for a bad ratio or a radiotap header it could not build).
    Failed(u32),
}

impl fmt::Display for TxCmdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable => write!(f, "wfb_tx control socket unreachable"),
            Self::Timeout => write!(f, "wfb_tx control command timed out"),
            Self::Io(e) => write!(f, "wfb_tx control socket io: {e}"),
            Self::BadLength { got, want } => {
                write!(f, "invalid response length {got} (want {want})")
            }
            Self::ReqIdMismatch { sent, got } => {
                write!(f, "response req_id {got} does not match request {sent}")
            }
            Self::Failed(rc) => write!(f, "wfb_tx rejected command (rc={rc})"),
        }
    }
}

impl std::error::Error for TxCmdError {}

impl From<std::io::Error> for TxCmdError {
    fn from(e: std::io::Error) -> Self {
        // A UDP datagram to a closed loopback port draws an ICMP port-unreachable
        // that surfaces on the next recv as ECONNREFUSED. That is the "no wfb_tx
        // listening" case the respawn fallback exists for, so classify it rather
        // than burying it in a generic io error.
        if e.kind() == std::io::ErrorKind::ConnectionRefused {
            Self::Unreachable
        } else {
            Self::Io(e)
        }
    }
}

/// What a decoded response carried past its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespBody {
    /// A `SET_*` acknowledgement — no body.
    Ack,
    Fec {
        fec_k: u8,
        fec_n: u8,
    },
    Radio(RadioSettings),
}

/// Encode a `set_fec` request. 7 bytes (`offsetof(cmd_req_t, u) + 2`).
pub fn encode_set_fec(req_id: u32, fec_k: u8, fec_n: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(REQ_HEADER_LEN + FEC_BODY_LEN);
    buf.extend_from_slice(&req_id.to_be_bytes());
    buf.push(CMD_SET_FEC);
    buf.push(fec_k);
    buf.push(fec_n);
    buf
}

/// Encode a `set_radio` request. 12 bytes (`offsetof(cmd_req_t, u) + 7`), field
/// order as declared in `cmd_set_radio` (`tx_cmd.h:19-28`).
pub fn encode_set_radio(req_id: u32, s: &RadioSettings) -> Vec<u8> {
    let mut buf = Vec::with_capacity(REQ_HEADER_LEN + RADIO_BODY_LEN);
    buf.extend_from_slice(&req_id.to_be_bytes());
    buf.push(CMD_SET_RADIO);
    buf.push(s.stbc);
    buf.push(u8::from(s.ldpc));
    buf.push(u8::from(s.short_gi));
    buf.push(s.bandwidth);
    buf.push(s.mcs_index);
    buf.push(u8::from(s.vht_mode));
    buf.push(s.vht_nss);
    buf
}

/// Encode a body-less request (`get_fec` / `get_radio`). 5 bytes.
pub fn encode_query(req_id: u32, cmd_id: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(REQ_HEADER_LEN);
    buf.extend_from_slice(&req_id.to_be_bytes());
    buf.push(cmd_id);
    buf
}

/// The exact response-body length `wfb_tx` sends for a command id.
const fn resp_body_len(cmd_id: u8) -> usize {
    match cmd_id {
        CMD_GET_FEC => FEC_BODY_LEN,
        CMD_GET_RADIO => RADIO_BODY_LEN,
        // SET_FEC / SET_RADIO acknowledge with a bare header, and so does the
        // ENOTSUP default arm (`tx.cpp:974-975`).
        _ => 0,
    }
}

/// Decode and validate a response datagram, in the reference client's order
/// (`tx_cmd.c:98-117`): header length, then `req_id`, then `rc`, then the exact
/// total length. Checking `rc` before the exact length matters — a rejection is
/// sent as a bare header regardless of which command was asked for, so a
/// length-first order would report every `EINVAL` as a framing error.
pub fn decode_response(cmd_id: u8, req_id: u32, buf: &[u8]) -> Result<RespBody, TxCmdError> {
    if buf.len() < RESP_HEADER_LEN {
        return Err(TxCmdError::BadLength {
            got: buf.len(),
            want: RESP_HEADER_LEN,
        });
    }
    let got_id = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if got_id != req_id {
        return Err(TxCmdError::ReqIdMismatch {
            sent: req_id,
            got: got_id,
        });
    }
    let rc = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if rc != 0 {
        return Err(TxCmdError::Failed(rc));
    }
    let want = RESP_HEADER_LEN + resp_body_len(cmd_id);
    if buf.len() != want {
        return Err(TxCmdError::BadLength {
            got: buf.len(),
            want,
        });
    }
    let body = &buf[RESP_HEADER_LEN..];
    Ok(match cmd_id {
        CMD_GET_FEC => RespBody::Fec {
            fec_k: body[0],
            fec_n: body[1],
        },
        CMD_GET_RADIO => RespBody::Radio(RadioSettings {
            stbc: body[0],
            ldpc: body[1] != 0,
            short_gi: body[2] != 0,
            bandwidth: body[3],
            mcs_index: body[4],
            vht_mode: body[5] != 0,
            vht_nss: body[6],
        }),
        _ => RespBody::Ack,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The struct offsets, cross-checked against the vendored packed structs by
    /// compiling `tx_cmd.h` directly: `offsetof(cmd_req_t,u)=5`,
    /// `offsetof(cmd_resp_t,u)=8`, `sizeof(cmd_set_fec)=2`,
    /// `sizeof(cmd_set_radio)=sizeof(cmd_get_radio)=7`.
    #[test]
    fn offsets_match_the_vendored_packed_structs() {
        assert_eq!(REQ_HEADER_LEN, 5);
        assert_eq!(RESP_HEADER_LEN, 8);
        assert_eq!(FEC_BODY_LEN, 2);
        assert_eq!(RADIO_BODY_LEN, 7);
    }

    /// `set_fec` is 7 bytes: the 5-byte header plus k and n. `wfb_tx`
    /// length-checks this exactly (`tx.cpp:855`) and answers EINVAL otherwise.
    /// The literal is byte-identical to what the vendored C struct produces.
    #[test]
    fn set_fec_request_is_seven_bytes_with_k_and_n() {
        let req = encode_set_fec(0x0102_0304, 8, 12);
        assert_eq!(req.len(), REQ_HEADER_LEN + FEC_BODY_LEN);
        assert_eq!(req, vec![0x01, 0x02, 0x03, 0x04, CMD_SET_FEC, 8, 12]);
    }

    /// `set_radio` is 12 bytes in `cmd_set_radio` declaration order. The literal
    /// is byte-identical to the vendored C struct filled the same way.
    #[test]
    fn set_radio_request_is_twelve_bytes_in_struct_order() {
        let s = RadioSettings {
            stbc: 1,
            ldpc: true,
            short_gi: true,
            bandwidth: 20,
            mcs_index: 5,
            vht_mode: false,
            vht_nss: 1,
        };
        let req = encode_set_radio(0xDEAD_BEEF, &s);
        assert_eq!(req.len(), REQ_HEADER_LEN + RADIO_BODY_LEN);
        assert_eq!(
            req,
            vec![0xDE, 0xAD, 0xBE, 0xEF, CMD_SET_RADIO, 1, 1, 1, 20, 5, 0, 1]
        );
    }

    /// The acceptance invariant: an MCS-only change still transmits bandwidth 20.
    /// `set_radio` replaces the WHOLE radiotap header, so omitting the width
    /// would retune the channel as a side effect of a rate change.
    #[test]
    fn mcs_only_change_still_encodes_bandwidth_20() {
        for mcs in 0..=7u8 {
            let s = RadioSettings::with_mcs(mcs);
            assert_eq!(s.bandwidth, PINNED_BANDWIDTH_MHZ);
            let req = encode_set_radio(1, &s);
            // Byte 8 is `bandwidth`: 4 id + 1 cmd + stbc + ldpc + short_gi.
            assert_eq!(req[8], 20, "mcs {mcs} must not change bandwidth");
            assert_eq!(req[9], mcs);
            // And nothing else moved off the spawn-time values.
            assert_eq!(req[5], 0, "stbc");
            assert_eq!(req[6], 0, "ldpc");
            assert_eq!(req[7], 0, "short_gi");
            assert_eq!(req[10], 0, "vht_mode");
            assert_eq!(req[11], 1, "vht_nss");
        }
    }

    /// A body-less query is exactly the 5-byte header (`tx.cpp:933`, `:951`).
    #[test]
    fn queries_are_header_only() {
        assert_eq!(encode_query(1, CMD_GET_FEC), vec![0, 0, 0, 1, CMD_GET_FEC]);
        assert_eq!(encode_query(1, CMD_GET_RADIO).len(), REQ_HEADER_LEN);
    }

    fn resp(req_id: u32, rc: u32, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&req_id.to_be_bytes());
        v.extend_from_slice(&rc.to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    /// A SET acknowledgement is a bare 8-byte header with rc 0.
    #[test]
    fn set_ack_decodes_to_ack() {
        let body = resp(7, 0, &[]);
        assert_eq!(
            decode_response(CMD_SET_RADIO, 7, &body).unwrap(),
            RespBody::Ack
        );
        assert_eq!(
            decode_response(CMD_SET_FEC, 7, &body).unwrap(),
            RespBody::Ack
        );
    }

    /// `rc` is big-endian: the server writes `htonl(errno)`. Decoding it
    /// little-endian would turn EINVAL (22) into 369098752 and, worse, would
    /// read a real rc of 0x16000000 as success.
    #[test]
    fn nonzero_rc_is_a_failure_and_is_big_endian() {
        let err = decode_response(CMD_SET_FEC, 7, &resp(7, 22, &[])).unwrap_err();
        match err {
            TxCmdError::Failed(rc) => assert_eq!(rc, 22),
            other => panic!("expected Failed(22), got {other:?}"),
        }
    }

    /// A rejection arrives as a bare header no matter which command was sent, so
    /// `rc` must be checked before the exact-length check or every EINVAL on a
    /// GET would be misreported as a framing error.
    #[test]
    fn rejected_query_reports_the_errno_not_a_length_error() {
        let err = decode_response(CMD_GET_RADIO, 9, &resp(9, 22, &[])).unwrap_err();
        assert!(
            matches!(err, TxCmdError::Failed(22)),
            "expected Failed(22), got {err:?}"
        );
    }

    /// A stale datagram carrying another request's id is rejected, not accepted
    /// as this command's answer.
    #[test]
    fn req_id_mismatch_is_rejected() {
        let err = decode_response(CMD_SET_FEC, 7, &resp(8, 0, &[])).unwrap_err();
        match err {
            TxCmdError::ReqIdMismatch { sent, got } => {
                assert_eq!((sent, got), (7, 8));
            }
            other => panic!("expected ReqIdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn short_and_overlong_responses_are_rejected() {
        // Shorter than the header.
        assert!(matches!(
            decode_response(CMD_SET_FEC, 1, &[0, 0, 0, 1, 0, 0, 0]).unwrap_err(),
            TxCmdError::BadLength { got: 7, want: 8 }
        ));
        // Right header, but a SET must not carry a body.
        assert!(matches!(
            decode_response(CMD_SET_FEC, 1, &resp(1, 0, &[9])).unwrap_err(),
            TxCmdError::BadLength { got: 9, want: 8 }
        ));
        // GET_RADIO needs exactly 7 body bytes.
        assert!(matches!(
            decode_response(CMD_GET_RADIO, 1, &resp(1, 0, &[0, 0, 0, 20, 3, 0])).unwrap_err(),
            TxCmdError::BadLength { got: 14, want: 15 }
        ));
    }

    /// `get_radio`'s body is the same 7-byte layout `set_radio` sends, so a
    /// readback round-trips a `RadioSettings` unchanged.
    #[test]
    fn radio_body_round_trips_through_encode_and_decode() {
        let s = RadioSettings {
            stbc: 2,
            ldpc: true,
            short_gi: false,
            bandwidth: 20,
            mcs_index: 3,
            vht_mode: true,
            vht_nss: 2,
        };
        let encoded = encode_set_radio(4, &s);
        // The request body and the response body are byte-identical layouts.
        let decoded = decode_response(CMD_GET_RADIO, 4, &resp(4, 0, &encoded[REQ_HEADER_LEN..]))
            .expect("decodes");
        assert_eq!(decoded, RespBody::Radio(s));
    }

    #[test]
    fn get_fec_body_decodes() {
        assert_eq!(
            decode_response(CMD_GET_FEC, 5, &resp(5, 0, &[8, 12])).unwrap(),
            RespBody::Fec {
                fec_k: 8,
                fec_n: 12
            }
        );
    }
}
