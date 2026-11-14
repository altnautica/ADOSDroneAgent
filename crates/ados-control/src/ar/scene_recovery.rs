//! The follow recovery items of layer `ar_motion`, drawn while follow-me
//! searches for a lost subject (`predicting`, `repositioning`, `searching`):
//!
//! * `fsearch`: the search ellipse as a ground polygon of [`SEARCH_POINTS`]
//!   points, clipped at the camera's near plane (renderers close it and fill
//!   it in the follow colour at 40 % alpha);
//! * `fvantage`: the vantage point the pilot chose, on the ground;
//! * `fpath` keyed `fpath:predicted`: the subject's predicted path, on the
//!   ground, after the vehicle's own `fpath`.

use serde_json::{json, Value};

use super::{polyline, pt, r3};
use crate::ar::project::View;

/// Points on the search ellipse.
const SEARCH_POINTS: usize = 24;

/// The search ellipse: centre, semi-axes, and the major axis as a bearing
/// from north (degrees).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchArea {
    pub center_ne: [f64; 2],
    pub semi_major_m: f64,
    pub semi_minor_m: f64,
    pub heading_deg: f64,
}

/// What a recovering follow publishes: `search_area`, `vantage_ne` and
/// `predicted_path_ne`, local NED north/east.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecoveryIn {
    pub area: Option<SearchArea>,
    pub vantage_ne: Option<[f64; 2]>,
    pub predicted_ne: Vec<[f64; 2]>,
}

/// The ellipse outline at ground depth `d`, as NED points.
fn ellipse(a: &SearchArea, d: f64) -> impl Iterator<Item = [f64; 3]> + '_ {
    let (sin_h, cos_h) = a.heading_deg.to_radians().sin_cos();
    (0..SEARCH_POINTS).map(move |i| {
        let (s, c) = (std::f64::consts::TAU * i as f64 / SEARCH_POINTS as f64).sin_cos();
        let (major, minor) = (a.semi_major_m * c, a.semi_minor_m * s);
        [
            a.center_ne[0] + major * cos_h - minor * sin_h,
            a.center_ne[1] + major * sin_h + minor * cos_h,
            d,
        ]
    })
}

/// A closed NED ring clipped at the near plane edge by edge, as image
/// points. A stretch behind the camera becomes a run along the near plane,
/// so the fill keeps the ground in front of the camera instead of cutting a
/// chord across it.
fn clipped_ring(view: &View, ring: &[[f64; 3]]) -> Vec<[f64; 2]> {
    let mut out: Vec<[f64; 2]> = Vec::new();
    for (i, &a) in ring.iter().enumerate() {
        let Some([p, q]) = view.segment(a, ring[(i + 1) % ring.len()]) else {
            continue;
        };
        // Unclipped, an edge starts where the last one ended; clipped, it
        // re-enters elsewhere on the near plane.
        if out.last() != Some(&p) {
            out.push(p);
        }
        out.push(q);
    }
    // A ring wholly in front ends where it began.
    if out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    out
}

/// The recovery items for one camera; `ground_d` is the ground's NED down.
pub(super) fn recovery_items(rec: &RecoveryIn, view: &View, ground_d: f64, items: &mut Vec<Value>) {
    if let Some(area) = rec.area.as_ref() {
        let outline: Vec<[f64; 3]> = ellipse(area, ground_d).collect();
        let ring: Vec<Value> = clipped_ring(view, &outline).into_iter().map(pt).collect();
        if ring.len() >= 3 {
            items.push(json!({"t": "fsearch", "k": "fsearch", "pts": ring}));
        }
    }
    if let Some(q) = rec
        .vantage_ne
        .and_then(|v| view.project([v[0], v[1], ground_d]))
    {
        items.push(json!({"t": "fvantage", "k": "fvantage", "x": r3(q[0]), "y": r3(q[1])}));
    }
    let path: Vec<[f64; 3]> = rec
        .predicted_ne
        .iter()
        .map(|p| [p[0], p[1], ground_d])
        .collect();
    let pts = polyline(view, &path);
    if pts.len() >= 2 {
        items.push(json!({"t": "fpath", "k": "fpath:predicted", "state": "clear", "pts": pts}));
    }
}
