//! A camera's pinhole model and body mount, enough to put a body-frame point
//! in its image and to cast a ray from an image point back into the body.
//!
//! The body frame is FRD (x forward, y right, z down). A camera is fixed to
//! the body, its optical axis pitched down from the nose by its mount pitch.
//! The `cameras[]` config spells that pitch as `mount_pitch_deg`, negative
//! looking down (a belly camera is `-90`), and its horizontal field of view as
//! `fov_deg`. Image coordinates are normalised, `0..1` across the width and
//! the height, y down; pixels are square.

use std::collections::HashMap;

use serde::Deserialize;

use super::feed::Attitude;

/// The horizontal field of view a camera without `fov_deg` is taken to have.
pub const DEFAULT_HFOV_DEG: f32 = 70.0;

/// One camera's projection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraGeometry {
    /// Horizontal field of view, degrees.
    pub hfov_deg: f32,
    /// Optical axis below the nose, degrees (positive looks down).
    pub pitch_down_deg: f32,
}

impl Default for CameraGeometry {
    fn default() -> Self {
        Self {
            hfov_deg: DEFAULT_HFOV_DEG,
            pitch_down_deg: 0.0,
        }
    }
}

/// A camera's projection resolved for one frame aspect.
#[derive(Debug, Clone, Copy)]
pub struct Projector {
    /// Focal length in widths and in heights.
    fx: f32,
    fy: f32,
    /// The camera's forward, right and down axes in the body frame.
    fwd: [f32; 3],
    right: [f32; 3],
    down: [f32; 3],
}

impl CameraGeometry {
    /// The projector for frames `aspect` = width / height.
    pub fn projector(&self, aspect: f32) -> Projector {
        let half = (self.hfov_deg.clamp(10.0, 170.0).to_radians() / 2.0).tan();
        let fx = 0.5 / half;
        let fy = fx * aspect.max(0.1);
        let (s, c) = self.pitch_down_deg.to_radians().sin_cos();
        Projector {
            fx,
            fy,
            fwd: [c, 0.0, s],
            right: [0.0, 1.0, 0.0],
            down: [-s, 0.0, c],
        }
    }
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

impl Projector {
    /// The depth of a body point along the optical axis, and where it lands
    /// in the image. `None` behind the camera.
    pub fn project(&self, p: [f32; 3]) -> Option<(f32, f32, f32)> {
        let z = dot(p, self.fwd);
        if z <= 1e-3 {
            return None;
        }
        let u = 0.5 + self.fx * dot(p, self.right) / z;
        let v = 0.5 + self.fy * dot(p, self.down) / z;
        Some((z, u, v))
    }

    /// The body-frame ray through image point `(u, v)`, scaled so its
    /// optical-axis component is 1: the point at optical depth `z` is
    /// `z * ray`.
    pub fn ray(&self, u: f32, v: f32) -> [f32; 3] {
        let x = (u - 0.5) / self.fx;
        let y = (v - 0.5) / self.fy;
        [
            self.fwd[0] + x * self.right[0] + y * self.down[0],
            self.fwd[1] + x * self.right[1] + y * self.down[1],
            self.fwd[2] + x * self.right[2] + y * self.down[2],
        ]
    }

    /// Where the line of sight through image point `(u, v)` at vehicle
    /// attitude `from` lands in the image at attitude `to`: the image shift
    /// of a pure turn of the vehicle (and the camera fixed to it). The
    /// vehicle's own travel between the two is left out, which holds for a
    /// subject several metres away seen across a few hundred milliseconds.
    /// `None` when the line of sight ends up behind the camera.
    pub fn turned(&self, u: f32, v: f32, from: &Attitude, to: &Attitude) -> Option<(f32, f32)> {
        let world = mat_vec(&body_to_ned(from), self.ray(u, v));
        let body = mat_t_vec(&body_to_ned(to), world);
        self.project(body).map(|(_, u, v)| (u, v))
    }
}

/// The rotation taking body (FRD) vectors into the local NED frame, for a
/// MAVLink attitude (yaw, then pitch, then roll).
fn body_to_ned(a: &Attitude) -> [[f32; 3]; 3] {
    let (sr, cr) = a.roll.sin_cos();
    let (sp, cp) = a.pitch.sin_cos();
    let (sy, cy) = a.yaw.sin_cos();
    [
        [cp * cy, sr * sp * cy - cr * sy, cr * sp * cy + sr * sy],
        [cp * sy, sr * sp * sy + cr * cy, cr * sp * sy - sr * cy],
        [-sp, sr * cp, cr * cp],
    ]
}

fn mat_vec(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [dot(m[0], v), dot(m[1], v), dot(m[2], v)]
}

fn mat_t_vec(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    let col = |j: usize| [m[0][j], m[1][j], m[2][j]];
    [dot(col(0), v), dot(col(1), v), dot(col(2), v)]
}

/// A camera's mount as its `cameras[]` entry states it: `fov_deg` (horizontal
/// field of view, degrees) and `mount_pitch_deg` (degrees, negative looking
/// down). Each is `None` when the entry leaves it out or gives a value no
/// camera can have (non-finite, or a field of view that is not positive).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CameraMount {
    pub fov_deg: Option<f32>,
    pub mount_pitch_deg: Option<f32>,
}

impl CameraMount {
    /// The projection, with [`DEFAULT_HFOV_DEG`] and a level mount standing in
    /// for what the entry leaves out.
    pub fn geometry(&self) -> CameraGeometry {
        CameraGeometry {
            hfov_deg: self.fov_deg.unwrap_or(DEFAULT_HFOV_DEG),
            pitch_down_deg: -self.mount_pitch_deg.unwrap_or(0.0),
        }
    }
}

/// Every camera's mount, by id, from the agent config's `cameras[]` entries.
/// The tap camera (`main`) takes the `primary` entry's mount when no entry is
/// named `main`. Unparseable text gives an empty map.
pub fn mounts_from_config(text: &str) -> HashMap<String, CameraMount> {
    #[derive(Deserialize)]
    struct Root {
        #[serde(default)]
        cameras: Vec<serde_norway::Value>,
    }
    #[derive(Deserialize)]
    struct Entry {
        id: Option<String>,
        role: Option<String>,
        fov_deg: Option<f32>,
        mount_pitch_deg: Option<f32>,
    }
    let Ok(root) = serde_norway::from_str::<Root>(text) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    let mut primary = None;
    for raw in root.cameras {
        let Ok(e) = serde_norway::from_value::<Entry>(raw) else {
            continue;
        };
        let Some(id) = e.id else { continue };
        let m = CameraMount {
            fov_deg: e.fov_deg.filter(|f| f.is_finite() && *f > 0.0),
            mount_pitch_deg: e.mount_pitch_deg.filter(|p| p.is_finite()),
        };
        if e.role.as_deref() == Some("primary") {
            primary = Some(m);
        }
        out.insert(id, m);
    }
    if let Some(m) = primary {
        out.entry(crate::cameras::TAP_CAMERA_ID.to_string())
            .or_insert(m);
    }
    out
}

/// Every mount's projection, by camera id.
pub fn geometries_of(mounts: &HashMap<String, CameraMount>) -> HashMap<String, CameraGeometry> {
    mounts
        .iter()
        .map(|(id, m)| (id.clone(), m.geometry()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forward_camera_puts_a_point_ahead_at_the_centre_and_right_to_the_right() {
        let p = CameraGeometry::default().projector(16.0 / 9.0);
        let (z, u, v) = p.project([10.0, 0.0, 0.0]).unwrap();
        assert!((z - 10.0).abs() < 1e-4 && (u - 0.5).abs() < 1e-6 && (v - 0.5).abs() < 1e-6);
        let (_, u, _) = p.project([10.0, 2.0, 0.0]).unwrap();
        assert!(u > 0.5);
        // At the edge of a 70° field of view.
        let edge = 10.0 * 35f32.to_radians().tan();
        let (_, u, _) = p.project([10.0, edge, 0.0]).unwrap();
        assert!((u - 1.0).abs() < 1e-4);
        assert!(p.project([-1.0, 0.0, 0.0]).is_none());
    }

    #[test]
    fn a_belly_camera_sees_the_point_below_at_the_centre_with_the_nose_at_the_top() {
        let g = CameraGeometry {
            hfov_deg: 90.0,
            pitch_down_deg: 90.0,
        };
        let p = g.projector(4.0 / 3.0);
        let (z, u, v) = p.project([0.0, 0.0, 5.0]).unwrap();
        assert!((z - 5.0).abs() < 1e-4 && (u - 0.5).abs() < 1e-5 && (v - 0.5).abs() < 1e-5);
        let (_, _, v) = p.project([1.0, 0.0, 5.0]).unwrap();
        assert!(v < 0.5, "a point ahead is toward the top");
    }

    #[test]
    fn the_ray_inverts_the_projection() {
        let g = CameraGeometry {
            hfov_deg: 80.0,
            pitch_down_deg: 30.0,
        };
        let p = g.projector(16.0 / 9.0);
        let pt = [6.0, -1.5, 2.0];
        let (z, u, v) = p.project(pt).unwrap();
        let r = p.ray(u, v);
        for i in 0..3 {
            assert!((r[i] * z - pt[i]).abs() < 1e-3);
        }
    }

    #[test]
    fn geometry_reads_fov_and_mount_and_gives_the_tap_camera_the_primary_entry() {
        let text = "cameras:\n  - id: front\n    role: primary\n    fov_deg: 82.5\n    mount_pitch_deg: -20\n  - id: belly\n    mount_pitch_deg: -90\n  - nonsense\n";
        let mounts = mounts_from_config(text);
        assert_eq!(
            mounts["front"],
            CameraMount {
                fov_deg: Some(82.5),
                mount_pitch_deg: Some(-20.0)
            }
        );
        assert_eq!(mounts["belly"].fov_deg, None);
        let g = geometries_of(&mounts);
        assert_eq!(g["front"].hfov_deg, 82.5);
        assert_eq!(g["front"].pitch_down_deg, 20.0);
        assert_eq!(g["belly"].hfov_deg, DEFAULT_HFOV_DEG);
        assert_eq!(g["belly"].pitch_down_deg, 90.0);
        assert_eq!(g["main"], g["front"]);
        assert!(mounts_from_config(": not yaml [").is_empty());
    }

    fn att(roll: f32, pitch: f32, yaw: f32) -> Attitude {
        Attitude { roll, pitch, yaw }
    }

    #[test]
    fn a_turn_moves_the_line_of_sight_across_the_image() {
        let g = CameraGeometry {
            hfov_deg: 87.0,
            pitch_down_deg: 15.0,
        };
        let p = g.projector(16.0 / 9.0);
        let level = att(0.0, 0.0, 1.0);
        // The nose pitches 0.2 rad down: what was at the centre is now above
        // it by the focal length times tan(0.2).
        let (u, v) = p.turned(0.5, 0.5, &level, &att(0.0, -0.2, 1.0)).unwrap();
        let fy = 0.5 / 43.5f32.to_radians().tan() * 16.0 / 9.0;
        assert!((u - 0.5).abs() < 1e-5);
        assert!((v - (0.5 - fy * 0.2f32.tan())).abs() < 1e-4, "{v}");
        // A level camera yawing right sees the scene go left.
        let p0 = CameraGeometry {
            hfov_deg: 87.0,
            pitch_down_deg: 0.0,
        }
        .projector(16.0 / 9.0);
        let (u, v) = p0.turned(0.5, 0.5, &level, &att(0.0, 0.0, 1.1)).unwrap();
        let fx = 0.5 / 43.5f32.to_radians().tan();
        assert!((u - (0.5 - fx * 0.1f32.tan())).abs() < 1e-4 && (v - 0.5).abs() < 1e-5);
        // No turn, no move; and there and back again is where it started.
        let a = att(0.05, -0.1, 2.0);
        let b = att(-0.08, 0.15, 2.3);
        let (u, v) = p.turned(0.3, 0.7, &a, &a).unwrap();
        assert!((u - 0.3).abs() < 1e-5 && (v - 0.7).abs() < 1e-5);
        let (u1, v1) = p.turned(0.3, 0.7, &a, &b).unwrap();
        let (u, v) = p.turned(u1, v1, &b, &a).unwrap();
        assert!((u - 0.3).abs() < 1e-4 && (v - 0.7).abs() < 1e-4);
        // Turned all the way round, the line of sight is behind the camera.
        assert!(p
            .turned(0.5, 0.5, &level, &att(0.0, 0.0, 1.0 + 3.0))
            .is_none());
    }
}
