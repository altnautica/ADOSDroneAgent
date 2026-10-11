//! The ground station's `mediamtx.yml`.
//!
//! Rendered by the fleet's single mediamtx config generator
//! ([`ados_video::mediamtx::mediamtx_config_yaml`], ground profile), with one
//! ground-only difference applied on top: path `main` reads the radio-decoded
//! RTP stream natively from UDP instead of waiting for a publisher. wfb_rx (via
//! the fan-out) pushes RTP-framed H.264 to `127.0.0.1:5600`; mediamtx listens
//! there itself, so no ffmpeg ingest sits in between. `rtpSDP` tells mediamtx
//! the payload type and codec, since a one-way radio broadcast has no RTSP
//! DESCRIBE to answer. The SPS/PPS arrive in-band with each IDR.

use ados_video::mediamtx::{mediamtx_config_yaml, ConfigParams, MediamtxProfile, MAIN_PATH};
use serde_norway::{Mapping, Value};

/// UDP port the fan-out delivers the decoded video RTP stream to.
pub const RTP_PORT: u16 = crate::fanout::MEDIAMTX_PORT;

/// The source mediamtx binds for path `main`.
pub fn main_source() -> String {
    format!("udp+rtp://127.0.0.1:{RTP_PORT}")
}

/// The SDP describing the RTP stream on [`RTP_PORT`]: H.264, payload type 96.
pub fn rtp_sdp() -> String {
    format!(
        "v=0\no=- 0 0 IN IP4 127.0.0.1\ns=ados-ground\nc=IN IP4 127.0.0.1\nt=0 0\n\
         m=video {RTP_PORT} RTP/AVP 96\na=rtpmap:96 H264/90000\n"
    )
}

/// Render the full ground `mediamtx.yml`, advertising `lan_ips` as WebRTC
/// host-candidate hints. Blocking (the shared renderer walks `/sys/class/net`).
pub fn render(lan_ips: &[String]) -> String {
    // The shared renderer emits `main` with the given source plus
    // `sourceOnDemand: true` for any non-publisher source. A UDP listener is
    // not an on-demand pull, so the path entry is replaced wholesale below.
    let streams = [(MAIN_PATH.to_string(), main_source())];
    let yaml = mediamtx_config_yaml(&ConfigParams::new(
        MediamtxProfile::Ground,
        lan_ips,
        &streams,
    ));
    let mut doc: Value =
        serde_norway::from_str(&yaml).expect("shared mediamtx renderer emits valid YAML");
    let paths = doc
        .get_mut("paths")
        .and_then(Value::as_mapping_mut)
        .expect("shared mediamtx renderer emits a paths mapping");
    paths.insert(Value::String(MAIN_PATH.to_string()), main_path_entry());
    serde_norway::to_string(&doc).expect("mediamtx config serializes")
}

/// The `paths.main` entry: a native UDP RTP source with its SDP.
fn main_path_entry() -> Value {
    let mut entry = Mapping::new();
    entry.insert(
        Value::String("source".to_string()),
        Value::String(main_source()),
    );
    entry.insert(
        Value::String("rtpSDP".to_string()),
        Value::String(rtp_sdp()),
    );
    Value::Mapping(entry)
}

#[cfg(test)]
mod tests;
