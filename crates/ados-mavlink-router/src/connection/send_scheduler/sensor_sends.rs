//! The sensor lane's sends toward the flight controller: TIMESYNC requests
//! and RTCM corrections.

use ados_protocol::mavlink::ardupilotmega::{GPS_RTCM_DATA_DATA, TIMESYNC_DATA};

use super::*;

impl FcConnection {
    /// Send a TIMESYNC request (`tc1 = 0`, `ts1 = ts1_ns`) to the local FC.
    /// Returns whether it was written; a node without a local FC writer sends
    /// nothing (an exchange over a relay would measure the radio, not the FC).
    pub async fn send_timesync(&self, ts1_ns: u64) -> bool {
        let msg = MavMessage::TIMESYNC(TIMESYNC_DATA {
            tc1: 0,
            ts1: ts1_ns as i64,
        });
        self.send_msg(&msg).await
    }

    /// Send one RTCM fragment as `GPS_RTCM_DATA` along the path a client
    /// command takes: the local FC writer, or on a node relaying a linked
    /// drone, its aux uplink.
    pub async fn send_rtcm_fragment(&self, data: GPS_RTCM_DATA_DATA) -> bool {
        let Ok(frame) = mavlink::serialize_v2(self.our_header(), &MavMessage::GPS_RTCM_DATA(data))
        else {
            return false;
        };
        let frame = self.agent_signing().sign(frame);
        self.send_client_bytes(&frame, ClientOrigin::Trusted, None)
            .await
    }
}
