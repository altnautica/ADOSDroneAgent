//! The board's front-panel button binding, read from the HAL board sidecar.
//!
//! A board declares its four buttons in its profile's `buttons:` block
//! (`{up, down, select, back}`), published verbatim into `/run/ados/board.json`.
//! Each value is the board's own GPIO number: the BCM line on a Raspberry Pi,
//! the Linux GPIO number (`bank * 32 + group * 8 + index`, the vendor pinout's
//! "GPIO number" column) on Rockchip. The SoC the sidecar names decides how a
//! number becomes a `(gpiochip, line)` pair; a board whose numbering is not
//! known here is never read, because requesting a guessed line can take a pin
//! away from the UART or bus that owns it.
//!
//! Only a Raspberry Pi falls back to the reference panel lines
//! ([`BUTTON_PINS`]) when it declares none. Any other board without a
//! declaration has no button reader.
//!
//! The sidecar may also carry `reserved_gpio`: the GPIO numbers the profile
//! binds to a bus, a console or another sensor. A button on one of them is
//! refused rather than requested.

/// Reference-panel lines (BCM), used only on a Raspberry Pi that declares no
/// buttons.
pub const BUTTON_PINS: [u32; 4] = [5, 6, 13, 19];

/// HAL board sidecar the supervisor writes for the detected board.
pub const BOARD_SIDECAR: &str = "/run/ados/board.json";

/// How a board's GPIO numbers map onto its character devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpioNumbering {
    /// BCM numbering: every header line is an offset on `/dev/gpiochip0`.
    RaspberryPi,
    /// Linux GPIO number: bank `n / 32` is `/dev/gpiochip<n / 32>`, offset
    /// `n % 32`.
    Rockchip,
}

impl GpioNumbering {
    /// The numbering of the board a sidecar describes, `None` when its SoC is
    /// one this reader has no mapping for.
    pub fn of_board(board: &serde_json::Value) -> Option<Self> {
        let field = |key: &str| board.get(key).and_then(|v| v.as_str()).unwrap_or("");
        let soc = field("soc").to_ascii_uppercase();
        if field("vendor") == "Raspberry Pi" || soc.starts_with("BCM") {
            Some(Self::RaspberryPi)
        } else if soc.starts_with("RK") {
            Some(Self::Rockchip)
        } else {
            None
        }
    }

    /// The `(chip index, line offset)` of GPIO number `gpio`.
    pub fn chip_line(self, gpio: u32) -> (u32, u32) {
        match self {
            Self::RaspberryPi => (0, gpio),
            Self::Rockchip => (gpio / 32, gpio % 32),
        }
    }
}

/// One button line to request: the board GPIO number (the press identity the
/// classifier and the wire carry) and where it lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ButtonLine {
    pub gpio: u32,
    pub chip: u32,
    pub line: u32,
}

impl ButtonLine {
    /// The character device carrying this line.
    pub fn chip_path(&self) -> String {
        format!("/dev/gpiochip{}", self.chip)
    }
}

/// Why the button reader does not run on this board. Each maps to one log
/// event name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ButtonsSkip {
    /// The sidecar is missing or does not parse.
    SidecarUnreadable,
    /// The board declares no buttons and is not a Raspberry Pi.
    NotDeclared,
    /// The board declares buttons but its GPIO numbering is unknown.
    NumberingUnknown { soc: String },
    /// A declared button sits on a line the board reserves for something else.
    Reserved { gpio: u32 },
}

impl ButtonsSkip {
    /// The structured log event name for this skip.
    pub fn event(&self) -> &'static str {
        match self {
            Self::SidecarUnreadable => "board_sidecar_unreadable",
            Self::NotDeclared => "buttons_not_declared",
            Self::NumberingUnknown { .. } => "buttons_numbering_unknown",
            Self::Reserved { .. } => "buttons_pin_reserved",
        }
    }
}

/// The `buttons` block of a board sidecar, top level or under a `board`
/// object, as GPIO numbers in role order (up, down, select, back). All four
/// roles must be present.
pub fn board_button_pins(sidecar_json: &str) -> Option<[u32; 4]> {
    let v: serde_json::Value = serde_json::from_str(sidecar_json).ok()?;
    declared_pins(board_object(&v))
}

fn board_object(v: &serde_json::Value) -> &serde_json::Value {
    match v.get("board") {
        Some(b) if b.is_object() => b,
        _ => v,
    }
}

fn declared_pins(board: &serde_json::Value) -> Option<[u32; 4]> {
    let block = board.get("buttons")?;
    let pin = |role: &str| -> Option<u32> { u32::try_from(block.get(role)?.as_u64()?).ok() };
    Some([pin("up")?, pin("down")?, pin("select")?, pin("back")?])
}

fn reserved_gpio(board: &serde_json::Value) -> Vec<u32> {
    board
        .get("reserved_gpio")
        .and_then(|r| r.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|g| g.as_u64().and_then(|g| u32::try_from(g).ok()))
                .collect()
        })
        .unwrap_or_default()
}

/// The four lines the reader requests on the board a sidecar describes, in
/// role order, or why it requests none.
pub fn button_lines(sidecar_json: &str) -> Result<[ButtonLine; 4], ButtonsSkip> {
    let v: serde_json::Value =
        serde_json::from_str(sidecar_json).map_err(|_| ButtonsSkip::SidecarUnreadable)?;
    let board = board_object(&v);
    let numbering = GpioNumbering::of_board(board);
    let pins = match (declared_pins(board), numbering) {
        (Some(pins), Some(_)) => pins,
        (Some(_), None) => {
            return Err(ButtonsSkip::NumberingUnknown {
                soc: board
                    .get("soc")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            })
        }
        (None, Some(GpioNumbering::RaspberryPi)) => BUTTON_PINS,
        (None, _) => return Err(ButtonsSkip::NotDeclared),
    };
    let reserved = reserved_gpio(board);
    if let Some(&gpio) = pins.iter().find(|p| reserved.contains(p)) {
        return Err(ButtonsSkip::Reserved { gpio });
    }
    let numbering = numbering.unwrap_or(GpioNumbering::RaspberryPi);
    Ok(pins.map(|gpio| {
        let (chip, line) = numbering.chip_line(gpio);
        ButtonLine { gpio, chip, line }
    }))
}

/// The button GPIO numbers of the board at `sidecar`, for matching a press to
/// its `B1`..`B4` slot. Falls back to [`BUTTON_PINS`] when the board has no
/// readable binding; a board without a reader publishes no presses, so the
/// fallback never resolves one.
pub fn board_pins_or_default(sidecar: &std::path::Path) -> [u32; 4] {
    std::fs::read_to_string(sidecar)
        .ok()
        .and_then(|text| button_lines(&text).ok())
        .map(|lines| lines.map(|l| l.gpio))
        .unwrap_or(BUTTON_PINS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROCK: &str = r#"{"vendor":"Radxa","soc":"RK3588",
        "buttons":{"up":63,"down":47,"select":103,"back":110},
        "reserved_gpio":[13,14,100,112,113,114,115]}"#;

    #[test]
    fn rockchip_numbers_split_into_bank_chip_and_offset() {
        assert_eq!(GpioNumbering::Rockchip.chip_line(100), (3, 4));
        assert_eq!(GpioNumbering::Rockchip.chip_line(13), (0, 13));
        let lines = button_lines(ROCK).unwrap();
        assert_eq!(
            lines.map(|l| (l.gpio, l.chip, l.line)),
            [(63, 1, 31), (47, 1, 15), (103, 3, 7), (110, 3, 14)]
        );
        assert_eq!(lines[2].chip_path(), "/dev/gpiochip3");
    }

    #[test]
    fn a_raspberry_pi_keeps_chip_zero_bcm_lines() {
        let declared = r#"{"vendor":"Raspberry Pi","soc":"BCM2711",
            "buttons":{"up":5,"down":6,"select":13,"back":19}}"#;
        let lines = button_lines(declared).unwrap();
        assert_eq!(
            lines.map(|l| (l.gpio, l.chip, l.line)),
            [(5, 0, 5), (6, 0, 6), (13, 0, 13), (19, 0, 19)]
        );
        // A Pi without a declaration reads the reference-panel lines.
        let bare = r#"{"vendor":"Raspberry Pi","soc":"BCM2712","buttons":null}"#;
        assert_eq!(button_lines(bare).unwrap().map(|l| l.gpio), BUTTON_PINS);
    }

    #[test]
    fn a_non_pi_board_without_buttons_has_no_reader_and_no_fallback() {
        let rock = r#"{"vendor":"Radxa","soc":"RK3588","buttons":null}"#;
        assert_eq!(button_lines(rock), Err(ButtonsSkip::NotDeclared));
        assert_eq!(ButtonsSkip::NotDeclared.event(), "buttons_not_declared");
        let unknown = r#"{"vendor":"unknown","soc":"unknown"}"#;
        assert_eq!(button_lines(unknown), Err(ButtonsSkip::NotDeclared));
        assert_eq!(
            button_lines("not json"),
            Err(ButtonsSkip::SidecarUnreadable)
        );
    }

    #[test]
    fn declared_buttons_on_an_unknown_numbering_are_not_read() {
        let a733 = r#"{"vendor":"Radxa","soc":"Allwinner A733",
            "buttons":{"up":1,"down":2,"select":3,"back":4}}"#;
        assert_eq!(
            button_lines(a733),
            Err(ButtonsSkip::NumberingUnknown {
                soc: "Allwinner A733".into()
            })
        );
    }

    #[test]
    fn a_button_on_a_reserved_line_is_refused() {
        let clash = r#"{"vendor":"Radxa","soc":"RK3588",
            "buttons":{"up":63,"down":13,"select":103,"back":110},
            "reserved_gpio":[13,14]}"#;
        assert_eq!(button_lines(clash), Err(ButtonsSkip::Reserved { gpio: 13 }));
    }

    #[test]
    fn the_buttons_block_reads_at_top_level_or_under_board() {
        assert_eq!(
            board_button_pins(r#"{"buttons":{"up":17,"down":27,"select":22,"back":23}}"#),
            Some([17, 27, 22, 23])
        );
        assert_eq!(
            board_button_pins(r#"{"board":{"buttons":{"up":1,"down":2,"select":3,"back":4}}}"#),
            Some([1, 2, 3, 4])
        );
        assert_eq!(board_button_pins(r#"{"buttons":{"up":17}}"#), None);
    }

    #[test]
    fn layout_pins_follow_the_board_binding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        std::fs::write(&path, ROCK).unwrap();
        assert_eq!(board_pins_or_default(&path), [63, 47, 103, 110]);
        assert_eq!(board_pins_or_default(&dir.path().join("none")), BUTTON_PINS);
    }
}
