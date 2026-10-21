//! Wi-Fi page data: scan results, saved networks, the join request body and
//! the classification of a refused join.

use ados_protocol::rest::RestError;
use serde_json::{json, Map, Value};

use crate::symbols::Symbols;

/// One network from `GET /api/v1/network/client/scan`.
#[derive(Debug, Clone, PartialEq)]
pub struct Network {
    pub ssid: String,
    /// 0-100 quality.
    pub signal: u8,
    /// NetworkManager-style label; `--` for an open network.
    pub security: String,
    pub in_use: bool,
}

impl Network {
    pub fn secured(&self) -> bool {
        !self.security.is_empty() && self.security != "--"
    }
}

/// The scan body's networks, in the agent's order (strongest first). Rows
/// without an SSID are dropped: they cannot be joined by name.
pub fn parse_scan(body: &Value) -> Vec<Network> {
    body.get("networks")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let ssid = r.get("ssid").and_then(Value::as_str)?.to_string();
                    if ssid.is_empty() {
                        return None;
                    }
                    let signal = r
                        .get("signal")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0)
                        .clamp(0.0, 100.0) as u8;
                    Some(Network {
                        ssid,
                        signal,
                        security: r
                            .get("security")
                            .and_then(Value::as_str)
                            .unwrap_or("--")
                            .to_string(),
                        in_use: r.get("in_use").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The lock glyph for a secured network.
pub fn lock_glyph(sym: &Symbols) -> &'static str {
    if sym.name == "ascii" {
        "*"
    } else {
        "🔒"
    }
}

/// The parts of one scan row as drawn: bars, ssid, lock (or blank), security
/// label, and the in-use mark (or blank).
pub fn network_cells(sym: &Symbols, n: &Network) -> [String; 5] {
    [
        sym.signal_bars(n.signal),
        n.ssid.clone(),
        if n.secured() { lock_glyph(sym).to_string() } else { String::new() },
        if n.secured() { n.security.clone() } else { "open".to_string() },
        if n.in_use { sym.ok.to_string() } else { String::new() },
    ]
}

/// One saved NetworkManager Wi-Fi profile.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub name: String,
    pub autoconnect: bool,
}

/// `GET /api/v1/network/client/configured` → the saved profiles.
pub fn parse_saved(body: &Value) -> Vec<Saved> {
    body.get("connections")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    Some(Saved {
                        name: r.get("name").and_then(Value::as_str)?.to_string(),
                        autoconnect: r.get("autoconnect").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `PUT /api/v1/network/client/join` body. The passphrase is sent only when
/// one was typed; `force` only on the hotspot-busy re-send.
pub fn join_body(ssid: &str, passphrase: Option<&str>, forced: bool) -> Value {
    let mut body = Map::new();
    body.insert("ssid".into(), json!(ssid));
    if let Some(p) = passphrase.filter(|p| !p.is_empty()) {
        body.insert("passphrase".into(), json!(p));
    }
    if forced {
        body.insert("force".into(), json!(true));
    }
    Value::Object(body)
}

/// Why a join was refused with a 409.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinConflict {
    /// The vehicle is armed: never forced.
    Armed,
    /// The hotspot holds the radio: a forced re-send may steal it.
    HotspotBusy,
}

/// Classify a failed join; `None` when it was not a 409.
pub fn join_conflict(err: &RestError) -> Option<JoinConflict> {
    if err.code() != Some(409) {
        return None;
    }
    let armed = err.body().and_then(|b| b.get("error")).and_then(Value::as_str) == Some("E_ARMED");
    Some(if armed {
        JoinConflict::Armed
    } else {
        JoinConflict::HotspotBusy
    })
}

/// Whether a status read failed because Wi-Fi control is absent on the node.
pub fn status_unavailable(code: Option<u16>, body: Option<&Value>) -> bool {
    code == Some(503)
        && body
            .and_then(|b| b.pointer("/detail/error/code"))
            .and_then(Value::as_str)
            == Some("E_WIFI_STATUS_UNAVAILABLE")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::by_name;

    fn scan() -> Value {
        json!({"networks":[
            {"ssid":"Bench","bssid":"aa","signal":82,"security":"WPA2","in_use":true},
            {"ssid":"Cafe Open","bssid":"bb","signal":30,"security":"--","in_use":false},
            {"ssid":"","bssid":"cc","signal":90,"security":"WPA2","in_use":false},
            {"ssid":"Weak","bssid":"dd","signal":5,"security":"WPA3","in_use":false}
        ]})
    }

    #[test]
    fn scan_rows_carry_bars_lock_and_in_use() {
        let nets = parse_scan(&scan());
        assert_eq!(nets.len(), 3, "the SSID-less row is dropped");
        let sym = by_name("unicode");
        let a = network_cells(sym, &nets[0]);
        assert_eq!(a[0], sym.signal_bars(82));
        assert_eq!(a[1], "Bench");
        assert_eq!(a[2], "🔒");
        assert_eq!(a[4], sym.ok);
        let b = network_cells(sym, &nets[1]);
        assert!(!nets[1].secured());
        assert_eq!(b[2], "");
        assert_eq!(b[3], "open");
        assert_eq!(b[4], "");
        let ascii = by_name("ascii");
        assert_eq!(network_cells(ascii, &nets[2])[2], "*");
        assert!(sym.signal_bars(82).chars().count() > sym.signal_bars(5).chars().count());
    }

    #[test]
    fn saved_profiles_parse() {
        let saved = parse_saved(&json!({"connections":[
            {"name":"Bench","type":"802-11-wireless","device":"wlan0","autoconnect":true},
            {"name":"Old","type":"802-11-wireless","device":null,"autoconnect":false}
        ]}));
        assert_eq!(saved, vec![
            Saved { name: "Bench".into(), autoconnect: true },
            Saved { name: "Old".into(), autoconnect: false },
        ]);
    }

    #[test]
    fn join_bodies() {
        assert_eq!(join_body("Bench", Some("secret12"), false), json!({"ssid":"Bench","passphrase":"secret12"}));
        assert_eq!(join_body("Cafe", None, false), json!({"ssid":"Cafe"}));
        assert_eq!(join_body("Cafe", Some(""), false), json!({"ssid":"Cafe"}));
        assert_eq!(
            join_body("Bench", Some("secret12"), true),
            json!({"ssid":"Bench","passphrase":"secret12","force":true})
        );
    }

    #[test]
    fn conflicts_split_armed_from_hotspot_busy() {
        let busy = RestError::Status {
            code: 409,
            body: Some(json!({"detail":{"error":{"code":"E_WLAN0_BUSY_AP_ACTIVE","message":"AP active"}},"needs_force":true})),
        };
        assert_eq!(join_conflict(&busy), Some(JoinConflict::HotspotBusy));
        let armed = RestError::Status {
            code: 409,
            body: Some(json!({"error":"E_ARMED","message":"Vehicle is armed","override":"force"})),
        };
        assert_eq!(join_conflict(&armed), Some(JoinConflict::Armed));
        let other = RestError::Status { code: 500, body: None };
        assert_eq!(join_conflict(&other), None);
    }

    #[test]
    fn unavailable_status_is_recognised() {
        let body = json!({"detail":{"error":{"code":"E_WIFI_STATUS_UNAVAILABLE","message":"x"}}});
        assert!(status_unavailable(Some(503), Some(&body)));
        assert!(!status_unavailable(Some(500), Some(&body)));
        assert!(!status_unavailable(Some(503), None));
    }
}
