//! When each occupied map cell was seen, as the σ the pose had then
//! ([`super::probe::Bar`]): the occupancy payloads and the pose's σ
//! bookkeeping.

use std::collections::HashSet;

use serde_json::Value;

use super::{tiles, Obstacles};

impl Obstacles {
    /// Ingest a `plugin.navigation.occupancy` payload (changed tiles only).
    pub fn ingest_occupancy(&mut self, payload: &Value) {
        let res = payload
            .get("res_m")
            .and_then(Value::as_f64)
            .map(|r| r as f32);
        let cells = payload
            .get("tile_cells")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        if res.is_some_and(|r| r != self.tile_res_m) || cells.is_some_and(|n| n != self.tile_cells)
        {
            // The kept cells now cover other ground: their sightings are
            // unknown.
            for t in self.tiles.values_mut() {
                t.forget_seen();
            }
        }
        if let Some(r) = res {
            self.tile_res_m = r;
        }
        if let Some(n) = cells {
            self.tile_cells = n;
        }
        let Some(list) = payload.get("tiles").and_then(Value::as_array) else {
            return;
        };
        if let Some(seq) = payload.get("seq").and_then(Value::as_u64) {
            if self.occ_seq.is_some_and(|last| seq != last + 1) {
                self.since_gap = Some(HashSet::new());
            }
            self.occ_seq = Some(seq);
        }
        // The σ the cells this payload newly occupies were seen at, at the
        // least; unknown (0, as old as the map) before any pose σ.
        let floor = match self.seen_floor {
            [Some(a), Some(b)] => a.min(b),
            [Some(s), None] | [None, Some(s)] => s,
            [None, None] => 0.0,
        };
        for t in list {
            let (Some(ix), Some(iy), Some(b64)) = (
                t.get("ix").and_then(Value::as_i64),
                t.get("iy").and_then(Value::as_i64),
                t.get("png_b64").and_then(Value::as_str),
            ) else {
                continue;
            };
            match tiles::decode(b64, self.tile_cells) {
                Ok(mut tile) => {
                    let key = (ix as i32, iy as i32);
                    let first_since_gap = self.since_gap.as_mut().is_some_and(|s| s.insert(key));
                    let floor = if first_since_gap { 0.0 } else { floor };
                    tile.note_seen(self.tiles.get(&key), floor);
                    self.tiles.insert(key, tile);
                }
                Err(e) => tracing::debug!(error = %e, ix, iy, "occupancy_tile_decode_failed"),
            }
        }
        self.seen_floor = [self.seen_floor[1], self.pose_sigma.map(|(s, _)| s)];
    }

    /// Fold a pose's σ (`sigma_xy_m`, already in `pose_sigma`) and health
    /// into the sighting bookkeeping.
    pub(super) fn note_pose_sigma(&mut self, payload: &Value, now_ms: u64) {
        if let Some((s, _)) = self.pose_sigma {
            if self.sigma_last.is_some_and(|l| s < l) {
                // The matcher's σ never falls within one map: a new map
                // began, and a cell it shares with the old one was seen in
                // it at σ `s` at the earliest.
                for t in self.tiles.values_mut() {
                    t.cap_seen(s);
                }
                self.seen_floor = [Some(s), Some(s)];
            }
            if self.sigma_last.is_some_and(|l| s > l) {
                self.sigma_rose_ms = Some(now_ms);
            }
            self.sigma_last = Some(s);
            self.sigma_low = Some(self.sigma_low.map_or(s, |l| l.min(s)));
            self.seen_floor[1] = Some(self.seen_floor[1].map_or(s, |f| f.min(s)));
        }
        self.pose_lost = payload.get("health").and_then(Value::as_str) == Some("lost");
    }

    /// The least σ any pose has published.
    pub fn sigma_low(&self) -> Option<f32> {
        self.sigma_low
    }

    /// The last pose reported its health `lost`.
    pub fn pose_lost(&self) -> bool {
        self.pose_lost
    }

    /// σ has not risen for `settle_ms`: the scans have matched the pose to
    /// the map since.
    pub fn sigma_settled(&self, now_ms: u64, settle_ms: u64) -> bool {
        self.sigma_rose_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= settle_ms)
    }
}
