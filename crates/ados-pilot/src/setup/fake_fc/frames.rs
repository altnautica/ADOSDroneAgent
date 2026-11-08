//! The fake FC's local frame and the frames it builds.

use ados_protocol::mavlink::ardupilotmega::*;

/// Where the fake FC's local origin is, degE7.
pub(super) const LAT0: i32 = 474_000_000;
pub(super) const LON0: i32 = 85_000_000;
/// Metres per degE7 of latitude, and the longitude scale at LAT0.
pub(super) const M_PER_E7: f32 = 0.011_131_95;
pub(super) const COS_LAT0: f32 = 0.676_9;

pub(super) fn latlon_of(n: f32, e: f32) -> (i32, i32) {
    (
        LAT0 + (n / M_PER_E7).round() as i32,
        LON0 + (e / (M_PER_E7 * COS_LAT0)).round() as i32,
    )
}

pub(super) fn ne_of(lat: i32, lon: i32) -> (f32, f32) {
    (
        (lat - LAT0) as f32 * M_PER_E7,
        (lon - LON0) as f32 * M_PER_E7 * COS_LAT0,
    )
}

pub(super) fn text(s: &str) -> MavMessage {
    MavMessage::STATUSTEXT(STATUSTEXT_DATA {
        severity: MavSeverity::MAV_SEVERITY_INFO,
        text: s.into(),
    })
}

/// A MAVLink 2 AUTOPILOT_VERSION frame carrying the `uid2` extension (the
/// fixed fields at their wire offsets, CRC not checked by the reader).
pub(super) fn version_frame(uid: u64, uid2: [u8; 18]) -> Vec<u8> {
    let mut payload = vec![0u8; 78];
    payload[8..16].copy_from_slice(&uid.to_le_bytes());
    payload[60..78].copy_from_slice(&uid2);
    while payload.last() == Some(&0) {
        payload.pop();
    }
    let mut f = vec![0xFD, payload.len() as u8, 0, 0, 0, 1, 1, 148, 0, 0];
    f.extend_from_slice(&payload);
    f.extend_from_slice(&[0, 0]);
    f
}

pub(super) fn ack(cmd: MavCmd, result: MavResult) -> MavMessage {
    MavMessage::COMMAND_ACK(COMMAND_ACK_DATA {
        command: cmd,
        result,
    })
}
