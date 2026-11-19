//! The plugin event-bus topics the nav hub subscribes to, the plugin each is
//! pinned to, and where an exported event goes.

use super::hub::NavKind;

/// The plugin that owns the pose, scan, lidar, occupancy, map-status and
/// `plugin.navigation.state` topics.
pub const NAVIGATION_ID: &str = "com.altnautica.navigation";
/// The plugin that publishes `follow.state` and the follow events.
pub const FOLLOW_ME_ID: &str = "com.altnautica.follow-me";

pub const TOPIC_POSE: &str = "plugin.navigation.pose";
pub const TOPIC_SCAN: &str = "plugin.navigation.scan";
/// The lidar driver's 1 Hz status (device, connected, health).
pub const TOPIC_LIDAR: &str = "plugin.navigation.lidar";
pub const TOPIC_OCCUPANCY: &str = "plugin.navigation.occupancy";
pub const TOPIC_MAP_STATUS: &str = "plugin.navigation.map_status";
pub const TOPIC_NAV_STATE: &str = "plugin.navigation.state";
pub const TOPIC_FOLLOW_STATE: &str = "follow.state";
pub const TOPIC_FOLLOW_REACQUIRED: &str = "follow.reacquired";
pub const TOPIC_FOLLOW_LOST: &str = "follow.lost";
pub const TOPIC_FOLLOW_STOPPED: &str = "follow.stopped";

/// Every topic the hub subscribes to on the export socket.
pub const EXPORT_TOPICS: [&str; 10] = [
    TOPIC_POSE,
    TOPIC_SCAN,
    TOPIC_LIDAR,
    TOPIC_OCCUPANCY,
    TOPIC_MAP_STATUS,
    TOPIC_NAV_STATE,
    TOPIC_FOLLOW_STATE,
    TOPIC_FOLLOW_REACQUIRED,
    TOPIC_FOLLOW_LOST,
    TOPIC_FOLLOW_STOPPED,
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
        TOPIC_POSE | TOPIC_SCAN | TOPIC_LIDAR | TOPIC_OCCUPANCY | TOPIC_MAP_STATUS
        | TOPIC_NAV_STATE => NAVIGATION_ID,
        TOPIC_FOLLOW_STATE | TOPIC_FOLLOW_REACQUIRED | TOPIC_FOLLOW_LOST | TOPIC_FOLLOW_STOPPED => {
            FOLLOW_ME_ID
        }
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
