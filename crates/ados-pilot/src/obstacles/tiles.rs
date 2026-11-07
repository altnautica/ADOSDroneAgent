//! Live occupancy tile decode: base64 → 8-bit grey PNG → cells.
//!
//! Cell values are 0 occupied, 254 free, 205 unknown. PNG row = `63 − lx`
//! (north up), column = `ly` (west left), where `lx`/`ly` are the tile-local
//! north/east cell indices.

use base64::Engine as _;

/// The occupied value on the wire.
pub const OCCUPIED: u8 = 0;

#[derive(Debug, Clone)]
pub struct Tile {
    cells: usize,
    /// Row-major PNG pixels.
    px: Vec<u8>,
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
    Ok(Tile { cells, px })
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
