//! The plugin event-bus topics the nav hub subscribes to, the plugin each is
//! pinned to, and where an exported event goes.

use super::hub::NavKind;

/// The plugin that owns the pose, scan, lidar, occupancy and map-status topics.
pub const WORLD_ENGINE_ID: &str = "com.altnautica.world-engine";
/// The plugin that publishes `nav.state`.
pub const VISION_NAV_ID: &str = "com.altnautica.vision-nav";
/// The plugin that publishes `follow.state` and the follow events.
pub const FOLLOW_ME_ID: &str = "com.altnautica.follow-me";

pub const TOPIC_POSE: &str = "plugin.world-engine.pose";
pub const TOPIC_SCAN: &str = "plugin.world-engine.scan";
/// The lidar driver's 1 Hz status (device, connected, health).
pub const TOPIC_LIDAR: &str = "plugin.world-engine.lidar";
pub const TOPIC_OCCUPANCY: &str = "plugin.world-engine.occupancy";
pub const TOPIC_MAP_STATUS: &str = "plugin.world-engine.map_status";
pub const TOPIC_NAV_STATE: &str = "nav.state";
pub const TOPIC_FOLLOW_STATE: &str = "follow.state";
pub const TOPIC_FOLLOW_REACQUIRED: &str = "follow.reacquired";
pub const TOPIC_FOLLOW_LOST: &str = "follow.lost";

/// Every topic the hub subscribes to on the export socket.
pub const EXPORT_TOPICS: [&str; 9] = [
    TOPIC_POSE,
    TOPIC_SCAN,
    TOPIC_LIDAR,
    TOPIC_OCCUPANCY,
    TOPIC_MAP_STATUS,
    TOPIC_NAV_STATE,
    TOPIC_FOLLOW_STATE,
    TOPIC_FOLLOW_REACQUIRED,
    TOPIC_FOLLOW_LOST,
];

/// Where one exported event goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routed {
    Latest(NavKind),
    Tiles,
    FollowEvent,
    /// The lidar driver status, kept for the sensor health surface.
    LidarStatus,
    Ignored,
}

/// Route an event by topic and host-stamped publisher. Each topic is pinned to
/// the plugin that owns it, so another plugin publishing a look-alike
/// `follow.state` (any plugin with `event.publish` may) never reaches the
/// operator's display.
pub fn route(topic: &str, publisher: &str) -> Routed {
    let owner = match topic {
        TOPIC_POSE | TOPIC_SCAN | TOPIC_LIDAR | TOPIC_OCCUPANCY | TOPIC_MAP_STATUS => {
            WORLD_ENGINE_ID
        }
        TOPIC_NAV_STATE => VISION_NAV_ID,
        TOPIC_FOLLOW_STATE | TOPIC_FOLLOW_REACQUIRED | TOPIC_FOLLOW_LOST => FOLLOW_ME_ID,
        _ => return Routed::Ignored,
    };
    if publisher != owner {
        return Routed::Ignored;
    }
    match topic {
        TOPIC_POSE => Routed::Latest(NavKind::Pose),
        TOPIC_SCAN => Routed::Latest(NavKind::Scan),
        TOPIC_MAP_STATUS => Routed::Latest(NavKind::Map),
        TOPIC_NAV_STATE => Routed::Latest(NavKind::Nav),
        TOPIC_FOLLOW_STATE => Routed::Latest(NavKind::Follow),
        TOPIC_OCCUPANCY => Routed::Tiles,
        TOPIC_LIDAR => Routed::LidarStatus,
        _ => Routed::FollowEvent,
    }
}
