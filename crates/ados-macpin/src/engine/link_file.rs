//! `.link` file rendering and `[Match]` block parsing.

use super::*;

/// Render the full `.link` body (pure). `match_block` is the body of the
/// `[Match]` section (without the `[Match]` header) and MUST name this one
/// adapter: `[Link] MACAddress=` is unconditional, so a wildcard match applies
/// the pinned address to EVERY interface on the box. `NamePolicy=kernel`
/// preserves the kernel interface name so a `wpa_supplicant@<iface>` binding
/// keeps working; an explicit `MACAddress=` sets the address unconditionally
/// (unlike `MACAddressPolicy=persistent`, which never fires on an adapter whose
/// `addr_assign_type` reads permanent while it actually randomizes).
pub fn render_link_file(layout: &PinLayout, match_block: &str, mac: &MacAddr) -> String {
    format!(
        "# Pin a stable MAC on an onboard adapter with no efuse MAC (it would\n\
# otherwise randomize each boot and churn the DHCP lease). Managed by the\n\
# {}; remove this file to revert. NamePolicy=kernel keeps the kernel\n\
# interface name so the wpa_supplicant binding survives. The [Match] block\n\
# names exactly one adapter: MACAddress= below is unconditional, so a glob\n\
# here would give every interface on this box the same address.\n\
[Match]\n\
{}\n\
\n\
[Link]\n\
NamePolicy=kernel\n\
MACAddress={}\n",
        layout.owner_label,
        match_block.trim_end(),
        mac
    )
}

/// The `[Match]` keys that identify ONE adapter. A pin `.link` is only safe
/// when at least one of these carries a glob-free value.
const IDENTITY_MATCH_KEYS: [&str; 4] =
    ["Path", "OriginalName", "PermanentMACAddress", "MACAddress"];

/// True when a `[Match]` block names one specific adapter (pure).
///
/// The pin used to COPY the `[Match]` of whichever stock `.link` won for the
/// interface, and on a stock systemd that is `99-default.link`, whose match is
/// `OriginalName=*`. Pairing that wildcard with our unconditional
/// `MACAddress=` handed the pinned address to every interface on the box: the
/// management NIC, a second radio, everything. So a pin is now refused
/// unless the match carries a glob-free identity key.
pub fn match_block_is_specific(match_block: &str) -> bool {
    match_block.lines().any(|raw| {
        let line = raw.trim();
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        let key = key.trim();
        let value = value.trim();
        IDENTITY_MATCH_KEYS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(key))
            && !value.is_empty()
            && !value.contains(['*', '?', '['])
            // A leading `!` is a systemd negation: "any adapter EXCEPT this
            // one", which is the opposite of specific.
            && !value.starts_with('!')
    })
}

/// Extract the `[Match]` section body from a `.link` file (pure). Returns the
/// lines under `[Match]` up to the next `[Section]` or EOF, joined by newlines.
pub fn parse_match_block(link_body: &str) -> Option<String> {
    let mut in_match = false;
    let mut lines: Vec<&str> = Vec::new();
    for raw in link_body.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            if line.eq_ignore_ascii_case("[Match]") {
                in_match = true;
                continue;
            }
            if in_match {
                break; // next section ends the Match block
            }
            continue;
        }
        if in_match && !line.is_empty() && !line.starts_with('#') {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// The `ID_PATH` value from a `udevadm info --query=property` body (pure).
///
/// `ID_PATH` is the stable per-port topology id (`platform-xhci-hcd.0-usb-0:1.3:1.0`)
/// that systemd's `.link` `Path=` key matches on, so it pins the adapter in a
/// given USB port rather than whatever currently answers to `wlan0`. Returns
/// `None` when the property is absent or carries a glob character (which would
/// make the match non-specific).
pub fn parse_udev_id_path(props: &str) -> Option<String> {
    for line in props.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("ID_PATH=") {
            let value = value.trim().trim_matches('"');
            if !value.is_empty() && !value.contains(['*', '?', '[']) {
                return Some(value.to_string());
            }
        }
    }
    None
}
