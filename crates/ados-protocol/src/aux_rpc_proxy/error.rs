//! Owned RPC responses and the proxy error type.

use super::*;

/// The response, owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcResponseOwned {
    /// The HTTP status code the drone's API returned.
    pub status: u16,
    /// The HTTP response body bytes the drone's API returned, reassembled from
    /// every fragment.
    pub body: Vec<u8>,
    /// The allow-listed response headers the drone sent with that body, in the
    /// order it sent them. Empty when the drone carried none — which is also
    /// what a drone older than the header block reports, so an empty list means
    /// "no headers crossed", never "the body has no type".
    ///
    /// Without these a relayed body is unusable for anything a consumer
    /// branches on: a downloaded artifact has no `content-type` and no
    /// `content-disposition` filename, a WHEP `Location` cannot route session
    /// teardown back through the proxy, and a `text/plain` error renders as a
    /// download.
    pub headers: Vec<aux_rpc::ResponseHeader>,
}

/// Why a call did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// The request body exceeds [`aux_rpc::MAX_REQUEST_BODY`]. The HTTP
    /// caller should surface a 413.
    TooLarge,
    /// A request field (target, path, content type, ticket) is too long to
    /// encode. The HTTP caller should surface a 400.
    Encode,
    /// The aux egress failed to send the request datagram.
    Send(String),
    /// No response arrived before the bound elapsed.
    Timeout,
    /// Some fragments arrived and the rest did not. Distinct from `Timeout`
    /// on purpose: "the drone never answered" and "the radio dropped a
    /// fragment" are different faults with different operator actions.
    Incomplete { received: usize, total: u16 },
    /// The consumer torn down between the request and the response, so the
    /// pending entry was removed. The caller may retry.
    ChannelClosed,
    /// Too many calls already in flight. Shedding here is honest: the lane
    /// cannot deliver them, and queueing would only convert a fast 503 into a
    /// slow timeout.
    Busy,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(f, "request body exceeds the relay limit"),
            Self::Encode => write!(f, "request field too long to encode"),
            Self::Send(e) => write!(f, "aux egress send failed: {e}"),
            Self::Timeout => write!(f, "no response before timeout"),
            Self::Incomplete { received, total } => {
                write!(f, "relay response incomplete: {received}/{total} fragments")
            }
            Self::ChannelClosed => write!(f, "aux consumer channel closed before response"),
            Self::Busy => write!(f, "relay proxy has too many calls in flight"),
        }
    }
}

impl std::error::Error for RpcError {}
