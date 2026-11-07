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
}

/// Every camera's geometry, by id, from the agent config's `cameras[]`
/// entries. The tap camera (`main`) takes the `primary` entry's geometry when
/// no entry is named `main`. Unparseable text gives an empty map: every camera
/// then uses [`CameraGeometry::default`].
pub fn geometries_from_config(text: &str) -> HashMap<String, CameraGeometry> {
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
        let g = CameraGeometry {
            hfov_deg: e
                .fov_deg
                .filter(|f| f.is_finite() && *f > 0.0)
                .unwrap_or(DEFAULT_HFOV_DEG),
            pitch_down_deg: -e.mount_pitch_deg.filter(|p| p.is_finite()).unwrap_or(0.0),
        };
        if e.role.as_deref() == Some("primary") {
            primary = Some(g);
        }
        out.insert(id, g);
    }
    if let Some(g) = primary {
        out.entry(crate::cameras::TAP_CAMERA_ID.to_string())
            .or_insert(g);
    }
    out
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
        let g = geometries_from_config(text);
        assert_eq!(g["front"].hfov_deg, 82.5);
        assert_eq!(g["front"].pitch_down_deg, 20.0);
        assert_eq!(g["belly"].hfov_deg, DEFAULT_HFOV_DEG);
        assert_eq!(g["belly"].pitch_down_deg, 90.0);
        assert_eq!(g["main"], g["front"]);
        assert!(geometries_from_config(": not yaml [").is_empty());
    }
}
