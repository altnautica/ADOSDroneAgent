//! Authenticated mesh join: the wpa_supplicant config (SAE for 802.11s,
//! IBSS-RSN for IBSS, both keyed from the deployment PSK), the private config
//! file, and the `iw` / sysfs checks that the interface is joined and bound to
//! batman-adv. There is no open-join path: without keying any radio that knows
//! the mesh id could join the batman-adv fabric.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::batctl;

/// Per carrier: the `iw ... set type` argument.
pub fn iw_set_type(carrier: &str) -> Option<&'static str> {
    match carrier {
        "802.11s" => Some("mp"),
        "ibss" => Some("ibss"),
        _ => None,
    }
}

/// Per carrier: the interface type `iw dev <if> info` reports once joined.
pub fn iw_info_type(carrier: &str) -> Option<&'static str> {
    match carrier {
        "802.11s" => Some("mesh point"),
        "ibss" => Some("IBSS"),
        _ => None,
    }
}

/// 2.4 GHz channel to centre frequency (channel 1 is 2412 MHz). Anything
/// outside 1..=13 is refused rather than mapped to a channel config and status
/// would not report.
pub fn channel_freq_mhz(channel: u8) -> Result<u32, String> {
    if !(1..=13).contains(&channel) {
        return Err(format!(
            "mesh channel {channel} is not a 2.4 GHz channel (1-13)"
        ));
    }
    Ok(2407 + u32::from(channel) * 5)
}

/// The passphrase every holder of the PSK derives: the key bytes hex-encoded,
/// cut at 63 characters (the WPA passphrase ceiling IBSS-RSN enforces). SAE
/// takes the same string as its `sae_password`.
pub fn mesh_passphrase(psk: &[u8]) -> String {
    let mut phrase = hex::encode(psk);
    phrase.truncate(63);
    phrase
}

/// Refuse a value that cannot sit verbatim inside a quoted wpa_supplicant
/// string: printable ASCII only, no quote or backslash, no edge whitespace,
/// within the length bounds.
pub fn check_conf_value(name: &str, value: &str, min: usize, max: usize) -> Result<(), String> {
    if !(min..=max).contains(&value.len()) {
        return Err(format!("{name} must be {min}-{max} characters"));
    }
    let printable = value
        .chars()
        .all(|c| (' '..='~').contains(&c) && c != '"' && c != '\\');
    if value != value.trim() || !printable {
        return Err(format!(
            "{name} must be printable ASCII without quotes, backslashes or edge whitespace"
        ));
    }
    Ok(())
}

/// The wpa_supplicant config for an authenticated join. 802.11s: mesh mode with
/// SAE and management-frame protection required. IBSS: ad-hoc with IBSS-RSN,
/// WPA2-PSK over CCMP.
pub fn wpa_supplicant_conf(
    carrier: &str,
    mesh_id: &str,
    passphrase: &str,
    freq_mhz: u32,
) -> Result<String, String> {
    check_conf_value("mesh_id", mesh_id, 1, 32)?;
    check_conf_value("mesh passphrase", passphrase, 8, 63)?;
    let lines: Vec<String> = match carrier {
        "802.11s" => vec![
            "network={".into(),
            format!("\tssid=\"{mesh_id}\""),
            "\tmode=5".into(),
            format!("\tfrequency={freq_mhz}"),
            "\tkey_mgmt=SAE".into(),
            format!("\tsae_password=\"{passphrase}\""),
            "\tieee80211w=2".into(),
            "}".into(),
        ],
        "ibss" => vec![
            // IBSS-RSN needs the driver-side scan/selection mode.
            "ap_scan=2".into(),
            "network={".into(),
            format!("\tssid=\"{mesh_id}\""),
            "\tmode=1".into(),
            format!("\tfrequency={freq_mhz}"),
            "\tproto=RSN".into(),
            "\tkey_mgmt=WPA-PSK".into(),
            "\tpairwise=CCMP".into(),
            "\tgroup=CCMP".into(),
            format!("\tpsk=\"{passphrase}\""),
            "}".into(),
        ],
        other => return Err(format!("unknown mesh carrier {other:?}")),
    };
    Ok(lines.join("\n") + "\n")
}

/// The per-interface config path under `wpa_dir`.
pub fn wpa_conf_path(wpa_dir: &Path, iface: &str) -> PathBuf {
    wpa_dir.join(format!("wpa_supplicant-{iface}.conf"))
}

/// Write `text` as a 0600 file in a 0700 directory (it carries the passphrase).
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(text.as_bytes())
}

/// Parse `iw dev <if> info`: the interface type and the joined mesh id / SSID.
pub fn parse_iw_info(text: &str) -> (Option<String>, Option<String>) {
    let mut iftype = None;
    let mut ssid = None;
    for line in text.lines() {
        let field = line.trim();
        if let Some(t) = field.strip_prefix("type ") {
            iftype = Some(t.trim().to_string());
        } else if let Some(s) = field.strip_prefix("ssid ") {
            ssid = Some(s.trim().to_string());
        }
    }
    (iftype, ssid)
}

/// Whether `iw` shows `iface` as the carrier's type joined to `mesh_id`.
pub async fn iface_joined(iface: &str, carrier: &str, mesh_id: &str) -> bool {
    let (rc, out, _) = batctl::run("iw", &["dev", iface, "info"], Duration::from_secs(5)).await;
    if rc != 0 {
        return false;
    }
    let (iftype, ssid) = parse_iw_info(&out);
    iftype.as_deref() == iw_info_type(carrier) && ssid.as_deref() == Some(mesh_id)
}

/// Whether `iface` is a batman-adv hard interface of `bat_iface`.
pub fn bat_member(sys_net: &Path, iface: &str, bat_iface: &str) -> bool {
    std::fs::read_to_string(sys_net.join(iface).join("batman_adv/mesh_iface"))
        .is_ok_and(|m| m.trim() == bat_iface)
}
