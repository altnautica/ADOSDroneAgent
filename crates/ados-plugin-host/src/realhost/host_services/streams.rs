//! Push-stream subscription host methods.

use super::*;

impl RealHost {
    pub(super) fn serve_mavlink_subscribe_stream(
        &self,
        _plugin_id: &str,
        _msg_name: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        self.mavlink.as_ref().map(|c| c.subscribe())
    }

    pub(super) fn serve_msp_subscribe_stream(
        &self,
        _plugin_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        // The router fans every FC->host MSP chunk out on one broadcast; there is
        // no per-message filter (MSP has no topic). When the MSP socket is not up
        // the slot is None and no stream arms, matching the MAVLink posture.
        self.msp.as_ref().map(|c| c.subscribe())
    }

    pub(super) fn serve_vision_subscribe_stream(
        &self,
        _plugin_id: &str,
        _camera_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        // The engine fans every camera's descriptors out on one broadcast; the
        // per-camera filter is applied plugin-side (the SDK subscribe_frames
        // callback drops a non-matching camera). The fanout outlives any one
        // engine connection, so a subscribe made while the engine is down
        // starts delivering once it connects. `None` only on a host with no
        // vision client wired.
        //
        // The engine pushes `vision.deliver` ONLY to a connection that asked for
        // it, so handing back a receiver is not enough: the upstream
        // subscription has to be armed too, or the fanout stays permanently
        // empty and every subscribing plugin sees silence with no error
        // anywhere. Armed lazily, and re-armed by the client on every new
        // engine connection, so a node whose plugins never ask for frames never
        // pays for the push.
        let client = self.vision.as_ref()?.clone();
        let rx = client.subscribe_frames();
        tokio::spawn(async move { client.arm_frame_push().await });
        Some(rx)
    }

    pub(super) fn serve_vision_subscribe_detection_stream(
        &self,
        _plugin_id: &str,
        _camera_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        // The engine fans every camera's detection batches out on one broadcast;
        // the per-camera filter is applied plugin-side (the SDK callback drops a
        // non-matching camera). Same reconnect posture as the frame stream.
        //
        // Same upstream-arming rule as the frame stream above.
        let client = self.vision.as_ref()?.clone();
        let rx = client.subscribe_detections();
        tokio::spawn(async move { client.arm_detection_push().await });
        Some(rx)
    }

    pub(super) fn serve_button_subscribe_stream(
        &self,
        _plugin_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        // One connection to the bus for the whole host process, shared by every
        // subscriber — the bus is a single socket and N plugins do not need N
        // connections to it. Started on first use rather than at construction so
        // a node whose plugins never ask for buttons never opens it.
        //
        // Always `Some`: an absent or down bus is a normal resting state (a
        // drone has no front panel), so the subscription succeeds and stays
        // quiet while the reader retries underneath. Returning `None` would make
        // every plugin treat "no buttons on this board" as an error to handle.
        Some(
            BUTTON_CLIENT
                .get_or_init(|| {
                    crate::button_client::ButtonClient::spawn(std::path::PathBuf::from(
                        crate::button_client::BUTTONS_SOCK,
                    ))
                })
                .subscribe(),
        )
    }

    pub(super) fn serve_display_zone_tap_stream(
        &self,
        _plugin_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        // One poller for the whole host process watches the display-tap sidecar,
        // shared by every subscriber — mirrors BUTTON_CLIENT's single-connection
        // philosophy. Started on first use rather than at construction so a node
        // whose plugins never subscribe to taps never starts it.
        //
        // Always `Some`: an absent display is a normal resting state (a drone has
        // no front panel), so the subscription succeeds and stays quiet while the
        // poller watches an empty file path.
        Some(
            DISPLAY_TAP_CLIENT
                .get_or_init(DisplayTapWatcher::spawn)
                .subscribe(),
        )
    }
}
