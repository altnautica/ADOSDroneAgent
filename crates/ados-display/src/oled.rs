//! SSD1306 / SH1106 monochrome OLED over I2C.
//!
//! Drives the 128x64 (or 128x32) mono OLED the ground-station OLED service
//! paints. The controller init is a short command sequence and the frame upload
//! is a page-by-page column write, so this talks the I2C character device
//! (`/dev/i2c-N`) directly via `nix` ioctls rather than dragging the
//! `ssd1306` / `sh1106` / `linux-embedded-hal` / `i2cdev` crate stack in.
//! Reasons: it keeps the aarch64-musl gate trivially pure-Rust (one transport,
//! `nix` only — already a workspace dep), and the init is ~6 bytes plus a
//! page-write loop. The two controllers differ only in a column offset (SH1106
//! is a 132-column part windowed to 128, so it starts at column 2).
//!
//! The frame packing (`pack_page_buffer`: a 1-bit-per-pixel canvas -> the
//! controller's page/column byte layout) is PURE and unit-tested; only the I2C
//! device open + ioctl write is Linux-gated.

/// I2C control byte that precedes a command stream (Co=0, D/C#=0).
pub const CTRL_CMD: u8 = 0x00;
/// I2C control byte that precedes a data stream (Co=0, D/C#=1).
pub const CTRL_DATA: u8 = 0x40;

/// Default 7-bit I2C address for both controllers.
pub const DEFAULT_I2C_ADDR: u16 = 0x3C;

/// Which controller is wired. They share the command set; SH1106 needs a
/// 2-column display offset because it is a 132-column die windowed to 128.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Controller {
    Ssd1306,
    Sh1106,
}

impl Controller {
    /// The starting column offset for a page write.
    pub fn column_offset(self) -> u8 {
        match self {
            Controller::Ssd1306 => 0,
            Controller::Sh1106 => 2,
        }
    }
}

/// Panel geometry. The OLED canvas is 128 wide; height is 64 or 32. Pages are
/// 8-pixel-tall horizontal bands, so `pages = height / 8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OledGeometry {
    pub width: u8,
    pub height: u8,
}

impl OledGeometry {
    pub const W128_H64: OledGeometry = OledGeometry {
        width: 128,
        height: 64,
    };
    pub const W128_H32: OledGeometry = OledGeometry {
        width: 128,
        height: 32,
    };

    /// Number of 8-pixel pages.
    pub fn pages(self) -> u8 {
        self.height / 8
    }

    /// Bytes in a full packed frame (one byte per column per page).
    pub fn frame_bytes(self) -> usize {
        self.width as usize * self.pages() as usize
    }
}

/// The init command sequence for `geom`. Mirrors the canonical SSD1306/SH1106
/// power-on sequence (charge pump on, addressing mode, contrast, display on).
/// The same sequence brings up both controllers; the SH1106 column offset is
/// applied at frame-upload time, not here.
pub fn init_commands(geom: OledGeometry) -> Vec<u8> {
    // Multiplex ratio tracks the panel height.
    let mux = geom.height - 1;
    // COM pin config: 0x12 for 64-row, 0x02 for 32-row panels.
    let com_pins = if geom.height == 32 { 0x02 } else { 0x12 };
    vec![
        0xAE, // display off
        0xD5, 0x80, // clock divide / osc freq
        0xA8, mux, // multiplex ratio
        0xD3, 0x00, // display offset
        0x40, // start line 0
        0x8D, 0x14, // charge pump on
        0x20, 0x00, // memory addressing mode: horizontal
        0xA1, // segment remap
        0xC8, // COM scan direction remapped
        0xDA, com_pins, // COM pins
        0x81, 0xCF, // contrast
        0xD9, 0xF1, // pre-charge
        0xDB, 0x40, // VCOMH deselect
        0xA4, // resume to RAM content
        0xA6, // normal (not inverted)
        0xAF, // display on
    ]
}

/// Pack a 1-bit-per-pixel canvas into the controller's page/column byte layout.
///
/// `canvas` is row-major, one byte per pixel, where any non-zero value is "on".
/// The output is `width * pages` bytes: for each page (8-pixel band) and each
/// column, one byte whose bit `b` is the pixel at `(col, page*8 + b)` — the
/// standard SSD1306/SH1106 vertical-byte layout. Returns `None` when the canvas
/// length does not match `width * height`.
pub fn pack_page_buffer(canvas: &[u8], geom: OledGeometry) -> Option<Vec<u8>> {
    let w = geom.width as usize;
    let h = geom.height as usize;
    if canvas.len() != w * h {
        return None;
    }
    let pages = geom.pages() as usize;
    let mut out = vec![0u8; w * pages];
    for page in 0..pages {
        for col in 0..w {
            let mut byte = 0u8;
            for bit in 0..8 {
                let y = page * 8 + bit;
                if canvas[y * w + col] != 0 {
                    byte |= 1 << bit;
                }
            }
            out[page * w + col] = byte;
        }
    }
    Some(out)
}

/// Build the per-page (command-header, data) pairs to upload a packed frame.
///
/// For each page this yields the column/page-address commands followed by that
/// page's `width` data bytes. The transport writes each command vec with the
/// [`CTRL_CMD`] prefix and each data vec with the [`CTRL_DATA`] prefix. Pulled
/// out so the upload sequence is testable without an I2C device.
pub fn page_upload_plan(
    packed: &[u8],
    geom: OledGeometry,
    controller: Controller,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let w = geom.width as usize;
    let pages = geom.pages() as usize;
    let col0 = controller.column_offset();
    let mut plan = Vec::with_capacity(pages);
    for page in 0..pages {
        let cmds = vec![
            0xB0 | page as u8,           // set page address
            col0 & 0x0F,                 // set lower column nibble (base 0x00)
            0x10 | ((col0 >> 4) & 0x0F), // set higher column nibble (base 0x10)
        ];
        let start = page * w;
        let data = packed[start..start + w].to_vec();
        plan.push((cmds, data));
    }
    plan
}

/// I2C transport for the OLED. The real implementation opens `/dev/i2c-N` and
/// issues the slave-address ioctl + writes; the test fake records the bytes.
pub trait I2cBus {
    /// Write `bytes` to the bound slave (the caller prepends the control byte).
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()>;
}

/// Send the init sequence then upload `packed` over `bus`. The control-byte
/// framing (CMD vs DATA) is applied here; the per-page plan comes from
/// [`page_upload_plan`]. Transport-agnostic, so it is exercised in tests with a
/// fake bus.
pub fn render_frame<B: I2cBus>(
    bus: &mut B,
    packed: &[u8],
    geom: OledGeometry,
    controller: Controller,
) -> std::io::Result<()> {
    for (cmds, data) in page_upload_plan(packed, geom, controller) {
        write_cmd(bus, &cmds)?;
        write_data(bus, &data)?;
    }
    Ok(())
}

/// Send a command stream (each prefixed by the command control byte).
pub fn write_cmd<B: I2cBus>(bus: &mut B, cmds: &[u8]) -> std::io::Result<()> {
    let mut frame = Vec::with_capacity(cmds.len() + 1);
    frame.push(CTRL_CMD);
    frame.extend_from_slice(cmds);
    bus.write(&frame)
}

/// Send a data stream (prefixed by the data control byte).
pub fn write_data<B: I2cBus>(bus: &mut B, data: &[u8]) -> std::io::Result<()> {
    let mut frame = Vec::with_capacity(data.len() + 1);
    frame.push(CTRL_DATA);
    frame.extend_from_slice(data);
    bus.write(&frame)
}

/// Run the controller init sequence over `bus`.
pub fn init<B: I2cBus>(bus: &mut B, geom: OledGeometry) -> std::io::Result<()> {
    write_cmd(bus, &init_commands(geom))
}

/// `I2C_SLAVE` ioctl request: select the target slave address for subsequent
/// reads/writes on an `/dev/i2c-N` fd. The `ioctl` request arg type differs
/// across libc flavors (glibc `c_ulong`, musl `c_int`), so it is cast at the
/// call site.
#[cfg(target_os = "linux")]
const I2C_SLAVE: u32 = 0x0703;

/// Raw `/dev/i2c-N` transport (Linux only). Binds the slave address once with
/// the `I2C_SLAVE` ioctl, then each write is a plain `write(2)` of the
/// control-prefixed buffer. Uses `nix::libc::ioctl` directly so no extra cargo
/// feature is needed; stays pure-Rust.
#[cfg(target_os = "linux")]
pub struct I2cDev {
    file: std::fs::File,
}

#[cfg(target_os = "linux")]
impl I2cDev {
    /// Open `/dev/i2c-<bus>` and bind `addr` (7-bit) as the active slave.
    pub fn open(bus: u8, addr: u16) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;

        let path = format!("/dev/i2c-{bus}");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;
        // SAFETY: I2C_SLAVE takes the address as the integer ioctl arg (not a
        // pointer); the fd is a freshly-opened i2c char device we own.
        let rc =
            unsafe { nix::libc::ioctl(file.as_raw_fd(), I2C_SLAVE as _, addr as nix::libc::c_int) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { file })
    }
}

#[cfg(target_os = "linux")]
impl I2cBus for I2cDev {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        self.file.write_all(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every byte buffer written, for asserting framing.
    #[derive(Default)]
    struct FakeBus {
        frames: Vec<Vec<u8>>,
    }

    impl I2cBus for FakeBus {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.frames.push(bytes.to_vec());
            Ok(())
        }
    }

    #[test]
    fn geometry_pages_and_frame_bytes() {
        assert_eq!(OledGeometry::W128_H64.pages(), 8);
        assert_eq!(OledGeometry::W128_H64.frame_bytes(), 128 * 8);
        assert_eq!(OledGeometry::W128_H32.pages(), 4);
        assert_eq!(OledGeometry::W128_H32.frame_bytes(), 128 * 4);
    }

    #[test]
    fn controller_column_offset() {
        assert_eq!(Controller::Ssd1306.column_offset(), 0);
        assert_eq!(Controller::Sh1106.column_offset(), 2);
    }

    #[test]
    fn init_sequence_tracks_height() {
        let c64 = init_commands(OledGeometry::W128_H64);
        // Mux ratio is height-1; COM pins 0x12 for 64-row.
        let mux_idx = c64.iter().position(|&b| b == 0xA8).unwrap();
        assert_eq!(c64[mux_idx + 1], 63);
        let com_idx = c64.iter().position(|&b| b == 0xDA).unwrap();
        assert_eq!(c64[com_idx + 1], 0x12);
        // Display-on terminates the sequence.
        assert_eq!(*c64.last().unwrap(), 0xAF);

        let c32 = init_commands(OledGeometry::W128_H32);
        let mux_idx = c32.iter().position(|&b| b == 0xA8).unwrap();
        assert_eq!(c32[mux_idx + 1], 31);
        let com_idx = c32.iter().position(|&b| b == 0xDA).unwrap();
        assert_eq!(c32[com_idx + 1], 0x02);
    }

    #[test]
    fn pack_page_buffer_vertical_byte_layout() {
        // 8x8 canvas: turn on the whole top page's column 0 (rows 0..8, col 0)
        // -> page 0, column 0 byte should be 0xFF; everything else 0.
        let geom = OledGeometry {
            width: 8,
            height: 8,
        };
        let mut canvas = vec![0u8; 8 * 8];
        for row in 0..8 {
            canvas[row * 8] = 1; // column 0, all rows
        }
        let packed = pack_page_buffer(&canvas, geom).unwrap();
        assert_eq!(packed.len(), 8); // 8 cols * 1 page
        assert_eq!(packed[0], 0xFF);
        assert!(packed[1..].iter().all(|&b| b == 0));
    }

    #[test]
    fn pack_page_buffer_single_pixel_sets_one_bit() {
        // Pixel at (col=3, y=5) -> page 0, column 3, bit 5 set.
        let geom = OledGeometry {
            width: 8,
            height: 8,
        };
        let mut canvas = vec![0u8; 8 * 8];
        canvas[5 * 8 + 3] = 1;
        let packed = pack_page_buffer(&canvas, geom).unwrap();
        assert_eq!(packed[3], 1 << 5);
    }

    #[test]
    fn pack_page_buffer_second_page_indexing() {
        // 8x16 -> 2 pages. Pixel at (col=0, y=8) is page 1 bit 0.
        let geom = OledGeometry {
            width: 8,
            height: 16,
        };
        let mut canvas = vec![0u8; 8 * 16];
        canvas[8 * 8] = 1; // y=8, col=0
        let packed = pack_page_buffer(&canvas, geom).unwrap();
        assert_eq!(packed.len(), 16); // 8 cols * 2 pages
                                      // page 1 starts at index width*1 = 8.
        assert_eq!(packed[8], 1 << 0);
    }

    #[test]
    fn pack_page_buffer_rejects_wrong_size() {
        let geom = OledGeometry::W128_H64;
        assert!(pack_page_buffer(&[0u8; 10], geom).is_none());
    }

    #[test]
    fn page_upload_plan_applies_sh1106_offset() {
        let geom = OledGeometry {
            width: 4,
            height: 8,
        };
        let packed = vec![1, 2, 3, 4];
        // SSD1306: column starts at 0 -> lower nibble 0x00, higher 0x10.
        let plan = page_upload_plan(&packed, geom, Controller::Ssd1306);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].0, vec![0xB0, 0x00, 0x10]);
        assert_eq!(plan[0].1, vec![1, 2, 3, 4]);
        // SH1106: column offset 2 -> lower nibble 0x02.
        let plan = page_upload_plan(&packed, geom, Controller::Sh1106);
        assert_eq!(plan[0].0, vec![0xB0, 0x02, 0x10]);
    }

    #[test]
    fn page_upload_plan_one_entry_per_page() {
        let geom = OledGeometry {
            width: 4,
            height: 16,
        };
        let packed = vec![0u8; 8]; // 4 cols * 2 pages
        let plan = page_upload_plan(&packed, geom, Controller::Ssd1306);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].0[0], 0xB0); // page 0
        assert_eq!(plan[1].0[0], 0xB1); // page 1
    }

    #[test]
    fn write_cmd_and_data_prefix_control_bytes() {
        let mut bus = FakeBus::default();
        write_cmd(&mut bus, &[0xAE]).unwrap();
        write_data(&mut bus, &[0x12, 0x34]).unwrap();
        assert_eq!(bus.frames[0], vec![CTRL_CMD, 0xAE]);
        assert_eq!(bus.frames[1], vec![CTRL_DATA, 0x12, 0x34]);
    }

    #[test]
    fn render_frame_emits_cmd_then_data_per_page() {
        let geom = OledGeometry {
            width: 4,
            height: 16,
        }; // 2 pages
        let packed = vec![10, 11, 12, 13, 20, 21, 22, 23];
        let mut bus = FakeBus::default();
        render_frame(&mut bus, &packed, geom, Controller::Ssd1306).unwrap();
        // 2 pages -> 4 writes (cmd, data, cmd, data).
        assert_eq!(bus.frames.len(), 4);
        assert_eq!(bus.frames[0][0], CTRL_CMD);
        assert_eq!(bus.frames[1], vec![CTRL_DATA, 10, 11, 12, 13]);
        assert_eq!(bus.frames[2][0], CTRL_CMD);
        assert_eq!(bus.frames[3], vec![CTRL_DATA, 20, 21, 22, 23]);
    }

    #[test]
    fn init_writes_the_command_block_once() {
        let mut bus = FakeBus::default();
        init(&mut bus, OledGeometry::W128_H64).unwrap();
        assert_eq!(bus.frames.len(), 1);
        assert_eq!(bus.frames[0][0], CTRL_CMD);
        assert_eq!(*bus.frames[0].last().unwrap(), 0xAF);
    }
}
