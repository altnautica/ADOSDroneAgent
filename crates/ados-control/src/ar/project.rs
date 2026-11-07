//! Pinhole projection of local-NED points into a camera image.
//!
//! The chain matches the follow-me extension's `ned_to_pixel`: body to NED is
//! `Rz(yaw)·Ry(pitch)·Rx(roll)`; the camera looks along its own forward axis,
//! tilted down by the mount pitch (or the gimbal pitch while gimbal data is
//! fresh). A stabilised gimbal holds its pitch and, in the earth frame, its
//! yaw against the horizon, so vehicle roll and pitch do not enter its chain.
//! `fx = (w/2)/tan(hfov/2)`, `fy = fx`, principal point at the centre, no
//! distortion. Points closer than [`NEAR_M`] in front of the camera do not
//! project; segments are clipped there.

/// The near clip distance along the camera axis, metres.
pub const NEAR_M: f64 = 0.5;

type Mat = [[f64; 3]; 3];

fn mul(a: &Mat, b: &Mat) -> Mat {
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

fn rx(a: f64) -> Mat {
    let (s, c) = a.sin_cos();
    [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
}

fn ry(a: f64) -> Mat {
    let (s, c) = a.sin_cos();
    [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
}

fn rz(a: f64) -> Mat {
    let (s, c) = a.sin_cos();
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}

/// The vehicle pose: local NED position and attitude in radians.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub ned: [f64; 3],
    pub roll: f64,
    pub pitch: f64,
    pub yaw: f64,
}

/// How the camera is pointed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Aim {
    /// Fixed to the airframe: tilted down by `tilt_down` (radians, + looks
    /// down; a belly camera is pi/2), yawed by `yaw` from the nose.
    Body { tilt_down: f64, yaw: f64 },
    /// A stabilised gimbal: pitch held against the horizon (`tilt_down`, +
    /// looks down); `yaw` is from the nose, or from north when `earth`.
    Gimbal { tilt_down: f64, yaw: f64, earth: bool },
}

/// One camera's image model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub w: f64,
    pub h: f64,
    pub hfov: f64,
    pub aim: Aim,
}

/// A camera placed in the world: everything a projection needs.
#[derive(Debug, Clone, Copy)]
pub struct View {
    origin: [f64; 3],
    /// Camera to NED.
    r: Mat,
    w: f64,
    h: f64,
    f: f64,
}

/// A point in the camera frame: `x` forward, `y` right, `z` down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CamPoint {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl View {
    pub fn new(cam: &Camera, pose: &Pose) -> Self {
        let r = match cam.aim {
            Aim::Body { tilt_down, yaw } => {
                let body = mul(&mul(&rz(pose.yaw), &ry(pose.pitch)), &rx(pose.roll));
                mul(&mul(&body, &rz(yaw)), &ry(-tilt_down))
            }
            Aim::Gimbal { tilt_down, yaw, earth } => {
                let heading = if earth { yaw } else { pose.yaw + yaw };
                mul(&rz(heading), &ry(-tilt_down))
            }
        };
        let f = (cam.w / 2.0) / (cam.hfov / 2.0).tan();
        Self { origin: pose.ned, r, w: cam.w, h: cam.h, f }
    }

    /// `p` (local NED) in the camera frame.
    pub fn to_cam(&self, p: [f64; 3]) -> CamPoint {
        let d = [p[0] - self.origin[0], p[1] - self.origin[1], p[2] - self.origin[2]];
        // R^T · d
        let c = |j: usize| (0..3).map(|i| self.r[i][j] * d[i]).sum::<f64>();
        CamPoint { x: c(0), y: c(1), z: c(2) }
    }

    /// Normalised image coordinates (0..1, y down) of a camera-frame point in
    /// front of the near plane; may fall outside 0..1 when off-frame.
    pub fn image(&self, c: CamPoint) -> Option<[f64; 2]> {
        if c.x < NEAR_M {
            return None;
        }
        let u = self.w / 2.0 + self.f * c.y / c.x;
        let v = self.h / 2.0 + self.f * c.z / c.x;
        Some([u / self.w, v / self.h])
    }

    /// [`Self::image`] of a NED point.
    pub fn project(&self, p: [f64; 3]) -> Option<[f64; 2]> {
        self.image(self.to_cam(p))
    }

    /// A NED segment clipped at the near plane and projected; `None` when it
    /// lies wholly behind it.
    pub fn segment(&self, a: [f64; 3], b: [f64; 3]) -> Option<[[f64; 2]; 2]> {
        let (mut ca, mut cb) = (self.to_cam(a), self.to_cam(b));
        if ca.x < NEAR_M && cb.x < NEAR_M {
            return None;
        }
        let clip = |p: CamPoint, q: CamPoint| {
            let t = (NEAR_M - p.x) / (q.x - p.x);
            CamPoint { x: NEAR_M, y: p.y + t * (q.y - p.y), z: p.z + t * (q.z - p.z) }
        };
        if ca.x < NEAR_M {
            ca = clip(ca, cb);
        } else if cb.x < NEAR_M {
            cb = clip(cb, ca);
        }
        Some([self.image(ca)?, self.image(cb)?])
    }

    /// The on-screen direction toward a camera-frame point, in screen radians
    /// (0 = +x, increasing clockwise because image y grows downward).
    pub fn screen_angle(c: CamPoint) -> f64 {
        if c.y.abs() < 1e-9 && c.z.abs() < 1e-9 {
            std::f64::consts::FRAC_PI_2
        } else {
            c.z.atan2(c.y)
        }
    }
}

/// Whether a normalised point is inside the frame.
pub fn on_frame(p: [f64; 2]) -> bool {
    (0.0..=1.0).contains(&p[0]) && (0.0..=1.0).contains(&p[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::FRAC_PI_2;

    fn level() -> Pose {
        Pose { ned: [0.0, 0.0, -10.0], roll: 0.0, pitch: 0.0, yaw: 0.0 }
    }

    fn cam(hfov_deg: f64, aim: Aim) -> Camera {
        Camera { w: 1280.0, h: 720.0, hfov: hfov_deg.to_radians(), aim }
    }

    const FORWARD: Aim = Aim::Body { tilt_down: 0.0, yaw: 0.0 };

    #[test]
    fn ten_metres_ahead_level_is_the_centre() {
        let v = View::new(&cam(90.0, FORWARD), &level());
        let p = v.project([10.0, 0.0, -10.0]).unwrap();
        assert!((p[0] - 0.5).abs() < 1e-9 && (p[1] - 0.5).abs() < 1e-9, "{p:?}");
    }

    #[test]
    fn one_metre_down_projects_below_centre() {
        let v = View::new(&cam(90.0, FORWARD), &level());
        let p = v.project([10.0, 0.0, -9.0]).unwrap();
        // fx = 640 / tan 45 = 640; v = 360 + 640 * 1 / 10 = 424 px.
        assert!((p[1] - 424.0 / 720.0).abs() < 1e-9, "{p:?}");
        assert!((p[0] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn behind_the_camera_is_none() {
        let v = View::new(&cam(90.0, FORWARD), &level());
        assert!(v.project([-10.0, 0.0, -10.0]).is_none());
    }

    #[test]
    fn a_belly_camera_looks_down() {
        let v = View::new(&cam(90.0, Aim::Body { tilt_down: FRAC_PI_2, yaw: 0.0 }), &level());
        let p = v.project([0.0, 0.0, 0.0]).unwrap();
        assert!((p[0] - 0.5).abs() < 1e-9 && (p[1] - 0.5).abs() < 1e-9, "{p:?}");
        // North of the drone is the top of a belly image.
        let north = v.project([2.0, 0.0, 0.0]).unwrap();
        assert!(north[1] < 0.5);
    }

    #[test]
    fn yaw_turns_the_view() {
        let pose = Pose { yaw: FRAC_PI_2, ..level() };
        let v = View::new(&cam(90.0, FORWARD), &pose);
        let p = v.project([0.0, 10.0, -10.0]).unwrap();
        assert!((p[0] - 0.5).abs() < 1e-9);
        // An earth-frame gimbal ignores the vehicle yaw.
        let g = View::new(&cam(90.0, Aim::Gimbal { tilt_down: 0.0, yaw: 0.0, earth: true }), &pose);
        assert!((g.project([10.0, 0.0, -10.0]).unwrap()[0] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn segments_clip_at_the_near_plane() {
        let v = View::new(&cam(90.0, FORWARD), &level());
        let s = v.segment([-5.0, 1.0, -10.0], [10.0, 1.0, -10.0]).unwrap();
        assert!(s[0][0] > 0.5 && s[1][0] > 0.5);
        assert!(v.segment([-5.0, 0.0, -10.0], [-1.0, 0.0, -10.0]).is_none());
    }

    #[test]
    fn screen_angle_is_clockwise_from_plus_x() {
        let down = View::screen_angle(CamPoint { x: -1.0, y: 0.0, z: 1.0 });
        assert!((down - FRAC_PI_2).abs() < 1e-9);
    }
}
