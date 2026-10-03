//! Nearby-network scan on the station radio through `iw`.
//!
//! `iw dev <iface> scan` runs a fresh active scan and prints one `BSS` block per
//! access point heard. On an interface currently serving as the access point
//! the kernel refuses a plain scan, so the `ap-force` flag is added when the AP
//! is up. The scan briefly leaves the operating channel; the AP's clients see a
//! short pause, not a disconnect.
//!
//! The parsed rows match the shape the REST layer serves:
//! `{ssid, bssid, signal (dBm), frequency_mhz, channel, security, in_use}`,
//! one row per SSID (the strongest access point wins), strongest first.
//! Hidden networks (empty SSID) are dropped: there is nothing to join by name.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use crate::cmd::CmdRunner;

/// A scan dwells on every channel; a dual-band radio takes several seconds.
const SCAN_TIMEOUT: Duration = Duration::from_secs(20);

/// Run the scan and parse it. `Err` carries a short reason when `iw` failed.
pub async fn scan(
    runner: &dyn CmdRunner,
    iface: &str,
    ap_force: bool,
) -> Result<Vec<Value>, String> {
    let mut argv = vec!["iw", "dev", iface, "scan"];
    if ap_force {
        argv.push("ap-force");
    }
    let out = runner.run(&argv, SCAN_TIMEOUT).await;
    if !out.ok() {
        let reason = out.stderr.trim();
        return Err(if reason.is_empty() {
            format!("scan failed on {iface} (exit {})", out.rc)
        } else {
            format!("scan failed on {iface}: {reason}")
        });
    }
    Ok(parse(&out.stdout)
        .into_iter()
        .map(ScanRow::into_json)
        .collect())
}

/// One access point from the scan output.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanRow {
    pub ssid: String,
    pub bssid: String,
    pub signal_dbm: i32,
    pub frequency_mhz: Option<u32>,
    pub security: &'static str,
    pub in_use: bool,
}

impl ScanRow {
    fn into_json(self) -> Value {
        json!({
            "ssid": self.ssid,
            "bssid": self.bssid,
            "signal": self.signal_dbm,
            "frequency_mhz": self.frequency_mhz,
            "channel": self.frequency_mhz.and_then(channel_for),
            "security": self.security,
            "in_use": self.in_use,
        })
    }
}

/// Parse `iw dev <iface> scan` output into one row per SSID, strongest first.
pub fn parse(output: &str) -> Vec<ScanRow> {
    let mut rows: Vec<ScanRow> = Vec::new();
    let mut current: Option<Block> = None;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("BSS ") {
            if let Some(block) = current.take() {
                rows.extend(block.finish());
            }
            current = Some(Block::new(rest));
            continue;
        }
        if let Some(block) = current.as_mut() {
            block.feed(line);
        }
    }
    if let Some(block) = current.take() {
        rows.extend(block.finish());
    }

    let mut best: HashMap<String, ScanRow> = HashMap::new();
    for row in rows {
        match best.get_mut(&row.ssid) {
            Some(held) => {
                let in_use = held.in_use || row.in_use;
                if row.signal_dbm > held.signal_dbm {
                    *held = row;
                }
                held.in_use = in_use;
            }
            None => {
                best.insert(row.ssid.clone(), row);
            }
        }
    }
    let mut out: Vec<ScanRow> = best.into_values().collect();
    out.sort_by(|a, b| {
        b.signal_dbm
            .cmp(&a.signal_dbm)
            .then_with(|| a.ssid.cmp(&b.ssid))
    });
    out
}

/// The 802.11 channel number for a centre frequency.
pub fn channel_for(freq_mhz: u32) -> Option<u32> {
    match freq_mhz {
        2484 => Some(14),
        2412..=2472 => Some((freq_mhz - 2407) / 5),
        5955..=7115 => Some((freq_mhz - 5950) / 5),
        5000..=5925 => Some((freq_mhz - 5000) / 5),
        _ => None,
    }
}

/// The fields gathered from one `BSS` block.
struct Block {
    bssid: String,
    associated: bool,
    ssid: Option<String>,
    signal_dbm: Option<i32>,
    frequency_mhz: Option<u32>,
    privacy: bool,
    rsn: bool,
    sae: bool,
    wpa: bool,
    in_rsn: bool,
}

impl Block {
    fn new(header: &str) -> Self {
        // `00:11:22:33:44:55(on wlan0) -- associated`
        let bssid = header
            .split(|c: char| c == '(' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        Self {
            bssid,
            associated: header.contains("-- associated"),
            ssid: None,
            signal_dbm: None,
            frequency_mhz: None,
            privacy: false,
            rsn: false,
            sae: false,
            wpa: false,
            in_rsn: false,
        }
    }

    fn feed(&mut self, line: &str) {
        let trimmed = line.trim();
        // An RSN element's suites are indented under it; any other top-level
        // field ends it.
        let top_level = line.starts_with('\t') && !line.starts_with("\t\t");
        if top_level {
            self.in_rsn = false;
        }
        if let Some(v) = trimmed.strip_prefix("freq:") {
            self.frequency_mhz = v.trim().parse::<f64>().ok().map(|f| f.round() as u32);
        } else if let Some(v) = trimmed.strip_prefix("signal:") {
            self.signal_dbm = v
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<f64>().ok())
                .map(|f| f.round() as i32);
        } else if let Some(v) = trimmed.strip_prefix("SSID:") {
            self.ssid = Some(unescape(v.trim()));
        } else if let Some(v) = trimmed.strip_prefix("capability:") {
            self.privacy = v.split_whitespace().any(|w| w == "Privacy");
        } else if trimmed.starts_with("RSN:") {
            self.rsn = true;
            self.in_rsn = true;
        } else if trimmed.starts_with("WPA:") {
            self.wpa = true;
        }
        if self.in_rsn && trimmed.contains("Authentication suites:") && trimmed.contains("SAE") {
            self.sae = true;
        }
    }

    fn finish(self) -> Option<ScanRow> {
        let ssid = self
            .ssid
            .filter(|s| !s.is_empty() && !s.chars().all(|c| c == '\0'))?;
        let security = if self.sae {
            "wpa3"
        } else if self.rsn {
            "wpa2"
        } else if self.wpa {
            "wpa"
        } else if self.privacy {
            "wep"
        } else {
            "open"
        };
        Some(ScanRow {
            ssid,
            bssid: self.bssid,
            signal_dbm: self.signal_dbm.unwrap_or(-100),
            frequency_mhz: self.frequency_mhz,
            security,
            in_use: self.associated,
        })
    }
}

/// Undo `iw`'s `\xNN` escaping of non-printable SSID bytes.
fn unescape(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'x') && i + 3 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 2..i + 4]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BSS aa:bb:cc:dd:ee:01(on wlan0) -- associated
\tfreq: 2437
\tcapability: ESS Privacy ShortSlotTime (0x0411)
\tsignal: -48.00 dBm
\tSSID: Field Net
\tRSN:\t * Version: 1
\t\t * Group cipher: CCMP
\t\t * Authentication suites: PSK
BSS aa:bb:cc:dd:ee:02(on wlan0)
\tfreq: 5180.0
\tcapability: ESS Privacy (0x0011)
\tsignal: -71.00 dBm
\tSSID: Field Net
\tRSN:\t * Version: 1
\t\t * Authentication suites: PSK
BSS 11:22:33:44:55:66(on wlan0)
\tfreq: 5745
\tcapability: ESS Privacy (0x0011)
\tsignal: -60.00 dBm
\tSSID: Lab\\x20Six
\tRSN:\t * Version: 1
\t\t * Authentication suites: SAE
BSS 11:22:33:44:55:77(on wlan0)
\tfreq: 2412
\tcapability: ESS (0x0001)
\tsignal: -80.00 dBm
\tSSID: Cafe
BSS 11:22:33:44:55:88(on wlan0)
\tfreq: 2462
\tsignal: -30.00 dBm
\tSSID: 
";

    #[test]
    fn rows_are_one_per_ssid_strongest_first_with_security_and_channel() {
        let rows = parse(SAMPLE);
        let names: Vec<&str> = rows.iter().map(|r| r.ssid.as_str()).collect();
        assert_eq!(
            names,
            vec!["Field Net", "Lab Six", "Cafe"],
            "hidden SSID dropped"
        );

        let field = &rows[0];
        assert_eq!(field.signal_dbm, -48);
        assert_eq!(field.bssid, "aa:bb:cc:dd:ee:01");
        assert_eq!(field.security, "wpa2");
        assert!(field.in_use);
        assert_eq!(field.frequency_mhz.and_then(channel_for), Some(6));

        let lab = &rows[1];
        assert_eq!(lab.security, "wpa3");
        assert_eq!(lab.frequency_mhz.and_then(channel_for), Some(149));
        assert!(!lab.in_use);

        assert_eq!(rows[2].security, "open");
        assert_eq!(rows[2].frequency_mhz.and_then(channel_for), Some(1));
    }
}
