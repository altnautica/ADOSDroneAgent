//! The live occupancy tile store behind `GET /api/v1/map/live`.
//!
//! The world-engine extension publishes only the tiles that changed each
//! second (`plugin.world-engine.occupancy`, `kind: "live"`). This store keeps
//! the latest PNG per tile and the producer seq it last changed at, so a client
//! catches up with `?since=<seq>` instead of replaying the stream.

use std::collections::HashMap;

use serde_json::{json, Value};

/// Most tiles held. A 64-cell tile at 5 cm is 3.2 m on a side, so this covers
/// about 13 000 m^2 of live map; the producer flushes far submaps itself. Past
/// the cap the tiles changed longest ago are dropped first.
pub(crate) const MAX_TILES: usize = 4096;

/// The frame the live map is drawn in (see the shared topic contract).
const DEFAULT_FRAME: &str = "map_frd";

#[derive(Debug, Clone)]
struct Tile {
    png_b64: String,
    seq: u64,
}

/// The live tiles of the current mapping session.
#[derive(Debug, Default)]
pub(crate) struct TileStore {
    frame: Option<String>,
    res_m: Option<f64>,
    tile_cells: Option<u64>,
    seq: u64,
    tiles: HashMap<(i64, i64), Tile>,
}

impl TileStore {
    /// Apply one live occupancy payload. A payload whose seq runs backwards
    /// (the producer restarted) or whose grid changed starts a new session.
    /// Anything other than a `kind: "live"` payload with a seq is ignored.
    pub(crate) fn apply(&mut self, payload: &Value) {
        if payload["kind"] != "live" {
            return;
        }
        let Some(seq) = payload["seq"].as_u64() else {
            return;
        };
        let res_m = payload["res_m"].as_f64();
        let tile_cells = payload["tile_cells"].as_u64();
        let restarted = seq < self.seq;
        let regridded =
            !self.tiles.is_empty() && (res_m != self.res_m || tile_cells != self.tile_cells);
        if restarted || regridded {
            self.tiles.clear();
            self.seq = 0;
        }
        self.res_m = res_m;
        self.tile_cells = tile_cells;
        self.frame = payload["frame"].as_str().map(str::to_owned);
        self.seq = self.seq.max(seq);
        for tile in payload["tiles"].as_array().into_iter().flatten() {
            let (Some(ix), Some(iy), Some(png)) = (
                tile["ix"].as_i64(),
                tile["iy"].as_i64(),
                tile["png_b64"].as_str(),
            ) else {
                continue;
            };
            self.tiles.insert(
                (ix, iy),
                Tile {
                    png_b64: png.to_owned(),
                    seq,
                },
            );
        }
        self.evict();
    }

    /// Drop the tiles changed longest ago until the store is within
    /// [`MAX_TILES`].
    fn evict(&mut self) {
        let excess = self.tiles.len().saturating_sub(MAX_TILES);
        if excess == 0 {
            return;
        }
        let mut by_age: Vec<((i64, i64), u64)> =
            self.tiles.iter().map(|(k, t)| (*k, t.seq)).collect();
        by_age.sort_by_key(|(k, seq)| (*seq, *k));
        for (key, _) in by_age.into_iter().take(excess) {
            self.tiles.remove(&key);
        }
    }

    /// The `GET /api/v1/map/live` body: every tile whose last change seq is
    /// after `since`, ordered by `(ix, iy)`. A `since` ahead of the current
    /// seq is a client still holding a previous session, so it gets every tile.
    pub(crate) fn body(&self, since: u64) -> Value {
        let since = if since > self.seq { 0 } else { since };
        let mut tiles: Vec<(&(i64, i64), &Tile)> =
            self.tiles.iter().filter(|(_, t)| t.seq > since).collect();
        tiles.sort_by_key(|(k, _)| **k);
        let tiles: Vec<Value> = tiles
            .into_iter()
            .map(|((ix, iy), t)| json!({"ix": ix, "iy": iy, "png_b64": t.png_b64}))
            .collect();
        json!({
            "seq": self.seq,
            "frame": self.frame.as_deref().unwrap_or(DEFAULT_FRAME),
            "res_m": self.res_m,
            "tile_cells": self.tile_cells,
            "tiles": tiles,
        })
    }
}
