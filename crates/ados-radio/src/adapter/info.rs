//! One `iw dev <iface> info` read, parsed into the live channel and TX power.
//!
//! The heartbeat needs both values every tick; reading them from one
//! subprocess instead of two halves the fork rate on the radio's hot path.

use super::run_cmd_output;
use super::txpower::parse_iface_txpower;

/// The live channel and TX power of an interface, from one `iw` read. Either
/// field is `None` when the `info` body does not carry it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IfaceInfo {
    pub channel: Option<u8>,
    pub txpower_dbm: Option<f32>,
}

/// Parse the `channel <N>` token out of an `iw <iface> info` body. The line
/// reads e.g. `\tchannel 149 (5745 MHz), width: 20 MHz, …`; the first integer
/// after the `channel` keyword is the channel number. Pure helper.
pub fn parse_iface_channel(info: &str) -> Option<u8> {
    for line in info.lines() {
        let mut toks = line.split_whitespace();
        while let Some(tok) = toks.next() {
            if tok == "channel" {
                if let Some(n) = toks.next() {
                    if let Ok(ch) = n.parse::<u8>() {
                        return Some(ch);
                    }
                }
            }
        }
    }
    None
}

/// Parse both fields out of one `iw dev <iface> info` body. Pure helper.
pub fn parse_iface_info(info: &str) -> IfaceInfo {
    IfaceInfo {
        channel: parse_iface_channel(info),
        txpower_dbm: parse_iface_txpower(info),
    }
}

/// Run `iw dev <iface> info` once and parse the channel and TX power. `None`
/// when `iw` cannot be run (missing tool, timeout).
pub async fn read_iface_info(iface: &str) -> Option<IfaceInfo> {
    let out = run_cmd_output("iw", &["dev", iface, "info"]).await.ok()?;
    Some(parse_iface_info(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iface_info_parses_channel_and_txpower() {
        let info = "Interface wlan1\n\tifindex 5\n\ttype monitor\n\
                    \tchannel 149 (5745 MHz), width: 20 MHz, center1: 5745 MHz\n\
                    \ttxpower 5.00 dBm\n";
        assert_eq!(
            parse_iface_info(info),
            IfaceInfo {
                channel: Some(149),
                txpower_dbm: Some(5.0),
            }
        );
    }

    #[test]
    fn iface_info_missing_fields_are_none() {
        assert_eq!(
            parse_iface_info("Interface wlan1\n\ttype managed\n"),
            IfaceInfo {
                channel: None,
                txpower_dbm: None,
            }
        );
        // A bare `channel` keyword with no number is None, not a panic.
        assert_eq!(parse_iface_channel("\tchannel\n"), None);
    }
}
