//! Live occupancy tile decode: base64 → 8-bit grey PNG → cells.
//!
//! Cell values are 0 occupied, 254 free, 205 unknown. PNG row = `63 − lx`
//! (north up), column = `ly` (west left), where `lx`/`ly` are the tile-local
//! north/east cell indices.

use base64::Engine as _;

/// The occupied value on the wire.
pub const OCCUPIED: u8 = 0;
/// The free value on the wire; every other value but [`OCCUPIED`] is unseen.
pub const FREE: u8 = 254;
/// The unseen value on the wire, and of a tile never received.
pub const UNSEEN: u8 = 205;

#[derive(Debug, Clone)]
pub struct Tile {
    cells: usize,
    /// Row-major PNG pixels.
    px: Vec<u8>,
    /// Each occupied cell (pixel index, ascending) with the pose's σ when it
    /// turned occupied ([`Self::note_seen`]).
    seen: Vec<(u32, f32)>,
}

impl Tile {
    /// Tile-local `(lx, ly)` of every occupied cell.
    pub fn occupied(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        let n = self.cells;
        self.px
            .iter()
            .enumerate()
            .filter(|(_, &v)| v == OCCUPIED)
            .map(move |(k, _)| {
                let (row, col) = (k / n, k % n);
                (n - 1 - row, col)
            })
    }

    /// Cells per side.
    pub fn cells(&self) -> usize {
        self.cells
    }

    /// The wire value of tile-local cell `(lx, ly)`.
    pub fn value(&self, lx: usize, ly: usize) -> u8 {
        self.px[(self.cells - 1 - lx) * self.cells + ly]
    }

    /// Keep, for each cell still occupied in `old` (the tile this one
    /// replaces), the σ it turned occupied at; a cell newly occupied takes
    /// `floor`, the least σ the pose had while it can have been seen.
    /// Earliest, not latest: a cell that stays occupied may never have been
    /// seen again, and an older σ only asks more clearance of it. An old
    /// tile of another size covered other ground: every cell is unknown.
    pub fn note_seen(&mut self, old: Option<&Tile>, floor: f32) {
        let resized = old.is_some_and(|o| o.cells != self.cells);
        let floor = if resized { 0.0 } else { floor };
        let kept = old.filter(|_| !resized).map_or(&[][..], |o| &o.seen[..]);
        let mut kept = kept.iter().peekable();
        self.seen.clear();
        for (k, _) in self.px.iter().enumerate().filter(|(_, &v)| v == OCCUPIED) {
            let k = k as u32;
            while kept.next_if(|(j, _)| *j < k).is_some() {}
            let s = kept.next_if(|(j, _)| *j == k).map_or(floor, |&(_, s)| s);
            self.seen.push((k, s));
        }
    }

    /// No cell was seen later than at σ `s`.
    pub fn cap_seen(&mut self, s: f32) {
        for (_, seen) in &mut self.seen {
            *seen = seen.min(s);
        }
    }

    /// Every cell's sighting is unknown.
    pub fn forget_seen(&mut self) {
        self.cap_seen(0.0);
    }

    /// The σ occupied cell `(lx, ly)` turned occupied at (0: unknown, as
    /// old as the map).
    pub fn seen_sigma(&self, lx: usize, ly: usize) -> f32 {
        let k = ((self.cells - 1 - lx) * self.cells + ly) as u32;
        self.seen
            .binary_search_by_key(&k, |&(j, _)| j)
            .map_or(0.0, |i| self.seen[i].1)
    }
}

/// Decode one tile. Filters 0–4 are undone by the PNG decoder.
pub fn decode(b64: &str, cells: usize) -> Result<Tile, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| e.to_string())?;
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.width as usize != cells || info.height as usize != cells {
        return Err(format!(
            "tile is {}x{}, expected {cells}x{cells}",
            info.width, info.height
        ));
    }
    let channels = info.color_type.samples();
    let px = buf[..info.buffer_size()]
        .chunks(channels)
        .map(|c| c[0])
        .collect();
    Ok(Tile {
        cells,
        px,
        seen: Vec::new(),
    })
}

#[cfg(test)]
pub fn encode_for_test(px: &[u8], cells: usize, filter: png::FilterType) -> String {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, cells as u32, cells as u32);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_filter(filter);
        enc.set_adaptive_filter(png::AdaptiveFilterType::NonAdaptive);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(px).unwrap();
    }
    base64::engine::general_purpose::STANDARD.encode(out)
}
