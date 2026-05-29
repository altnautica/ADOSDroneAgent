//! Visual-inertial pose injection toward the flight controller.
//!
//! A vision-navigation plugin estimates the vehicle's pose from camera frames
//! (visual-inertial odometry) and feeds it to the autopilot. The autopilot
//! consumes that estimate over MAVLink as a `VISION_POSITION_ESTIMATE` (a pose
//! sample with Euler attitude) or an `ODOMETRY` message (pose plus body-frame
//! twist, ROS REP 147 layout). Before this helper, every VIO plugin
//! hand-assembled those frames byte by byte; here the SDK builds them from the
//! shared ardupilotmega codec and sends them over the host's MAVLink path under
//! the visual-odometry component id, so the FC attributes the estimate to a
//! vision source.
//!
//! The plugin registers itself once as the visual-odometry component
//! ([`VIO_COMPONENT_ID`]) via `ctx.mavlink.register_component`, then calls
//! [`super::VisionClient::inject_pose`] / `inject_odometry` per estimate.

use ados_protocol::mavlink::{
    ardupilotmega::{MavFrame, ODOMETRY_DATA, VISION_POSITION_ESTIMATE_DATA},
    serialize_v2, MavHeader, MavMessage, MavlinkError,
};

/// MAVLink component id a vision plugin registers as when it feeds pose to the
/// flight controller. `MAV_COMP_ID_VISUAL_INERTIAL_ODOMETRY` (197) in the
/// MAVLink component-id enum: the FC tags the estimate as coming from a vision
/// source rather than a peripheral or the GCS.
pub const VIO_COMPONENT_ID: i64 = 197;

/// Length of a MAVLink 6x6 pose cross-covariance upper-triangle (the
/// `covariance` field of `VISION_POSITION_ESTIMATE` and the `pose_covariance`
/// field of `ODOMETRY`): 21 row-major entries over the states x, y, z, roll,
/// pitch, yaw.
pub const POSE_COVARIANCE_LEN: usize = 21;

/// A pose estimate a vision plugin feeds to the flight controller.
///
/// `position` is the local NED position in metres `(x, y, z)`. `orientation`
/// is the body-to-local rotation as a quaternion `(w, x, y, z)` (`(1, 0, 0, 0)`
/// is the null rotation). `timestamp_us` is the capture time in microseconds on
/// the same clock the frame descriptor stamps with (UNIX epoch or time since
/// boot; the FC infers which from the magnitude). `covariance`, when present,
/// is the 21-entry upper triangle of the 6x6 pose cross-covariance; `None`
/// sends the MAVLink "unknown" marker (NaN in the first element).
#[derive(Debug, Clone, PartialEq)]
pub struct Pose {
    pub position: (f32, f32, f32),
    pub orientation: (f32, f32, f32, f32),
    pub timestamp_us: u64,
    pub covariance: Option<[f32; POSE_COVARIANCE_LEN]>,
}

impl Pose {
    /// A pose with an identity orientation and no covariance, at the origin.
    pub fn identity(timestamp_us: u64) -> Self {
        Self {
            position: (0.0, 0.0, 0.0),
            orientation: (1.0, 0.0, 0.0, 0.0),
            timestamp_us,
            covariance: None,
        }
    }

    /// The covariance as MAVLink wants it: the supplied 21 entries, or the
    /// unknown marker (NaN in element 0) when absent.
    fn covariance_field(&self) -> [f32; POSE_COVARIANCE_LEN] {
        match self.covariance {
            Some(c) => c,
            None => {
                let mut c = [0.0f32; POSE_COVARIANCE_LEN];
                c[0] = f32::NAN;
                c
            }
        }
    }

    /// Euler attitude `(roll, pitch, yaw)` in radians from the quaternion
    /// `(w, x, y, z)`, for `VISION_POSITION_ESTIMATE` which carries Euler
    /// angles rather than a quaternion. Standard aerospace 3-2-1 (ZYX) sequence.
    fn euler_rpy(&self) -> (f32, f32, f32) {
        let (w, x, y, z) = self.orientation;
        // roll (x-axis rotation)
        let sinr_cosp = 2.0 * (w * x + y * z);
        let cosr_cosp = 1.0 - 2.0 * (x * x + y * y);
        let roll = sinr_cosp.atan2(cosr_cosp);
        // pitch (y-axis rotation), clamped at the poles to avoid NaN from asin.
        let sinp = 2.0 * (w * y - z * x);
        let pitch = if sinp.abs() >= 1.0 {
            (std::f32::consts::FRAC_PI_2).copysign(sinp)
        } else {
            sinp.asin()
        };
        // yaw (z-axis rotation)
        let siny_cosp = 2.0 * (w * z + x * y);
        let cosy_cosp = 1.0 - 2.0 * (y * y + z * z);
        let yaw = siny_cosp.atan2(cosy_cosp);
        (roll, pitch, yaw)
    }

    /// Build the `VISION_POSITION_ESTIMATE` message for this pose. The attitude
    /// is converted from the quaternion to Euler angles, matching the message's
    /// field layout (`usec`, position, and Euler roll/pitch/yaw). The pose
    /// covariance rides the `ODOMETRY` path; `VISION_POSITION_ESTIMATE` carries
    /// position and attitude only.
    pub fn to_vision_position_estimate(&self) -> MavMessage {
        let (roll, pitch, yaw) = self.euler_rpy();
        MavMessage::VISION_POSITION_ESTIMATE(VISION_POSITION_ESTIMATE_DATA {
            usec: self.timestamp_us,
            x: self.position.0,
            y: self.position.1,
            z: self.position.2,
            roll,
            pitch,
            yaw,
        })
    }
}

/// A full odometry estimate: pose plus body-frame linear and angular velocity.
///
/// Wraps a [`Pose`] and adds the twist a `VISION_POSITION_ESTIMATE` cannot
/// carry. `linear_velocity` is `(vx, vy, vz)` in m/s and `angular_velocity` is
/// `(rollspeed, pitchspeed, yawspeed)` in rad/s, both in the child (body)
/// frame. `velocity_covariance`, when present, is the 21-entry upper triangle
/// of the 6x6 velocity cross-covariance.
#[derive(Debug, Clone, PartialEq)]
pub struct Odometry {
    pub pose: Pose,
    pub linear_velocity: (f32, f32, f32),
    pub angular_velocity: (f32, f32, f32),
    pub velocity_covariance: Option<[f32; POSE_COVARIANCE_LEN]>,
}

impl Odometry {
    /// Build the `ODOMETRY` message. The reference frames default to local NED
    /// for the pose and body NED for the twist, matching a forward-facing VIO
    /// source feeding ArduPilot.
    pub fn to_odometry(&self) -> MavMessage {
        let (w, x, y, z) = self.pose.orientation;
        let vel_cov = match self.velocity_covariance {
            Some(c) => c,
            None => {
                let mut c = [0.0f32; POSE_COVARIANCE_LEN];
                c[0] = f32::NAN;
                c
            }
        };
        MavMessage::ODOMETRY(ODOMETRY_DATA {
            time_usec: self.pose.timestamp_us,
            x: self.pose.position.0,
            y: self.pose.position.1,
            z: self.pose.position.2,
            q: [w, x, y, z],
            vx: self.linear_velocity.0,
            vy: self.linear_velocity.1,
            vz: self.linear_velocity.2,
            rollspeed: self.angular_velocity.0,
            pitchspeed: self.angular_velocity.1,
            yawspeed: self.angular_velocity.2,
            pose_covariance: self.pose.covariance_field(),
            velocity_covariance: vel_cov,
            frame_id: MavFrame::MAV_FRAME_LOCAL_NED,
            child_frame_id: MavFrame::MAV_FRAME_BODY_NED,
        })
    }
}

/// Serialize a vision message into a complete MAVLink v2 frame, stamped with
/// the visual-odometry component id so the host's router and the FC attribute
/// it to a vision source. The system id is left to the host (a registered
/// component sends from the agent's own system id); the sequence is `0` because
/// the host's router owns sequence numbering on the outbound link.
pub(crate) fn frame_for(msg: &MavMessage) -> Result<Vec<u8>, MavlinkError> {
    let header = MavHeader {
        system_id: 1,
        component_id: VIO_COMPONENT_ID as u8,
        sequence: 0,
    };
    serialize_v2(header, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::mavlink::parse_v2;

    #[test]
    fn identity_pose_builds_a_vision_position_estimate_frame() {
        let pose = Pose::identity(1_700_000_000_000);
        let frame = frame_for(&pose.to_vision_position_estimate()).unwrap();
        assert_eq!(frame[0], 0xFD); // MAVLink v2 magic.
        let (header, msg) = parse_v2(&frame).unwrap();
        assert_eq!(header.component_id, VIO_COMPONENT_ID as u8);
        match msg {
            MavMessage::VISION_POSITION_ESTIMATE(d) => {
                assert_eq!(d.usec, 1_700_000_000_000);
                assert_eq!((d.x, d.y, d.z), (0.0, 0.0, 0.0));
                // Identity quaternion -> zero Euler angles.
                assert!(d.roll.abs() < 1e-6);
                assert!(d.pitch.abs() < 1e-6);
                assert!(d.yaw.abs() < 1e-6);
            }
            other => panic!("expected VISION_POSITION_ESTIMATE, got {other:?}"),
        }
    }

    #[test]
    fn quaternion_yaw_maps_to_euler_yaw() {
        // 90 deg yaw about Z: q = (cos45, 0, 0, sin45).
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let pose = Pose {
            position: (1.0, 2.0, -3.0),
            orientation: (s, 0.0, 0.0, s),
            timestamp_us: 42,
            covariance: None,
        };
        match pose.to_vision_position_estimate() {
            MavMessage::VISION_POSITION_ESTIMATE(d) => {
                assert!((d.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
                assert!(d.roll.abs() < 1e-5);
                assert!(d.pitch.abs() < 1e-5);
                assert_eq!((d.x, d.y, d.z), (1.0, 2.0, -3.0));
            }
            other => panic!("expected VISION_POSITION_ESTIMATE, got {other:?}"),
        }
    }

    #[test]
    fn odometry_carries_quaternion_and_twist() {
        let odo = Odometry {
            pose: Pose {
                position: (5.0, 6.0, 7.0),
                orientation: (1.0, 0.0, 0.0, 0.0),
                timestamp_us: 99,
                covariance: Some([0.1; POSE_COVARIANCE_LEN]),
            },
            linear_velocity: (0.5, -0.5, 0.0),
            angular_velocity: (0.01, 0.02, 0.03),
            velocity_covariance: None,
        };
        let frame = frame_for(&odo.to_odometry()).unwrap();
        let (header, msg) = parse_v2(&frame).unwrap();
        assert_eq!(header.component_id, VIO_COMPONENT_ID as u8);
        match msg {
            MavMessage::ODOMETRY(d) => {
                assert_eq!(d.time_usec, 99);
                assert_eq!(d.q, [1.0, 0.0, 0.0, 0.0]);
                assert_eq!((d.vx, d.vy, d.vz), (0.5, -0.5, 0.0));
                assert_eq!((d.rollspeed, d.pitchspeed, d.yawspeed), (0.01, 0.02, 0.03));
                assert!((d.pose_covariance[0] - 0.1).abs() < 1e-6);
                assert!(d.velocity_covariance[0].is_nan());
            }
            other => panic!("expected ODOMETRY, got {other:?}"),
        }
    }
}
