//! Stable-MAC pinning for network adapters that have no efuse MAC and so
//! randomize their hardware address on every driver load.
//!
//! Some USB WiFi chipsets ship without a MAC burned into efuse, so their driver
//! generates a fresh random MAC each time it loads. On a DHCP network that means
//! a new lease — and a new IP — every boot, which makes the box hard to find and
//! breaks long-running operations when the address moves mid-flight. The fix is
//! to pin a stable, locally-administered MAC the adapter keeps across boots, via
//! a high-priority `systemd-networkd` `.link` drop-in that sets the address on
//! the next boot without ever touching the live interface.
//!
//! This crate is the shared engine for that feature, used by the install-time
//! provisioning step and the always-on supervisor reconciler:
//!   * the quirk table of chipsets known to lack an efuse MAC ([`is_quirk_randomizer`]),
//!   * the allowlist of chipsets known to carry a stable efuse MAC ([`is_known_stable_efuse`]),
//!   * the deterministic per-board MAC derivation ([`derive_pinned_mac`]),
//!   * the on-disk state-file contract ([`MacPinsState`]) the heartbeat reads,
//!   * and (in [`engine`]) the sysfs enumeration + `.link` provisioning.
//!
//! Detection deliberately does NOT trust two signals that look authoritative but
//! lie in practice (both proven on an AICSemi AIC8800D80): the kernel's
//! `addr_assign_type` reads `0` (permanent) while the adapter actually
//! randomizes, and the random MAC uses a real-looking vendor OUI with the
//! locally-administered bit clear. So randomization is identified by chipset
//! VID:PID (quirk table) or by observing the MAC change across boots for a stable
//! device identity, never by `addr_assign_type` or the LAA bit.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod engine;

/// A USB device identity. Defined locally (rather than pulled from a heavier
/// crate) to keep this engine lean enough for the bootstrap installer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UsbId {
    pub vid: u16,
    pub pid: u16,
}

/// A chipset-quirk entry: a USB vendor id plus an optional product id. A `None`
/// product matches every product from that vendor (a vendor-family match).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacQuirk {
    pub vid: u16,
    /// `Some(pid)` matches one product; `None` matches the whole vendor family.
    pub pid: Option<u16>,
    /// Human label for logs and the operator surface.
    pub label: &'static str,
}

/// Chipsets that have no efuse MAC and randomize their address each driver load.
///
/// Keep this list small and evidence-driven — a false entry would pin an adapter
/// that does not need it. Grow it only as boards are validated.
///
/// AICSemi (vid 0xa69c) USB WiFi (the AIC8800 family) is the founding case:
/// proven on an AIC8800D80 (`a69c:8d81`) that re-randomizes its MAC every boot
/// and churns the DHCP lease. The whole vendor is matched because the AIC8800
/// series shares this trait across its product ids.
pub const MAC_QUIRKS: &[MacQuirk] = &[MacQuirk {
    vid: 0xa69c,
    pid: None,
    label: "AICSemi AIC8800-family USB WiFi (no efuse MAC)",
}];

/// Chipsets known to carry a stable efuse MAC. These are NEVER pinned, even
/// before the cross-boot learner has confirmed them, so a known-good radio's
/// hardware address is never overwritten.
///
/// The RTL88xx family (Realtek, vid 0x0bda) is the radio adapter the platform
/// uses for the WFB link; it has a real efuse MAC.
pub const KNOWN_STABLE_EFUSE: &[MacQuirk] = &[MacQuirk {
    vid: 0x0bda,
    pid: None,
    label: "Realtek RTL88xx USB WiFi (efuse MAC)",
}];

fn table_match(table: &[MacQuirk], id: UsbId) -> Option<&'static str> {
    table.iter().find_map(|q| {
        if q.vid == id.vid && q.pid.map(|p| p == id.pid).unwrap_or(true) {
            Some(q.label)
        } else {
            None
        }
    })
}

/// Return the quirk label when `id` is a known no-efuse randomizer, else `None`.
pub fn is_quirk_randomizer(id: UsbId) -> Option<&'static str> {
    table_match(MAC_QUIRKS, id)
}

/// True when `id` is a chipset known to carry a stable efuse MAC (never pin it).
pub fn is_known_stable_efuse(id: UsbId) -> bool {
    table_match(KNOWN_STABLE_EFUSE, id).is_some()
}

/// A 48-bit MAC address. Serialized as the canonical lowercase colon-hex string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    /// True when the unicast bit (LSB of the first octet) is clear.
    pub fn is_unicast(&self) -> bool {
        self.0[0] & 0x01 == 0
    }

    /// True when the locally-administered bit (second-LSB of the first octet) is
    /// set — a self-assigned address that cannot collide with a vendor OUI.
    pub fn is_locally_administered(&self) -> bool {
        self.0[0] & 0x02 != 0
    }

    /// Parse a colon- or dash-separated 6-octet MAC (case-insensitive).
    pub fn parse(s: &str) -> Option<MacAddr> {
        let mut bytes = [0u8; 6];
        let mut n = 0;
        for part in s.split([':', '-']) {
            if n >= 6 {
                return None;
            }
            bytes[n] = u8::from_str_radix(part.trim(), 16).ok()?;
            n += 1;
        }
        if n == 6 {
            Some(MacAddr(bytes))
        } else {
            None
        }
    }
}

impl std::fmt::Display for MacAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

impl Serialize for MacAddr {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for MacAddr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        MacAddr::parse(&s).ok_or_else(|| serde::de::Error::custom("invalid MAC address"))
    }
}

/// Derive the deterministic pinned MAC for a board.
///
/// The address is `02:` (locally-administered + unicast) followed by the first
/// five bytes of `sha256(machine_id[:salt])`. Keying on the stable
/// `/etc/machine-id` makes the value deterministic per board and collision-free
/// across boards. `salt` is empty for the only randomizing adapter on a box
/// (reproducing the historical value bit-for-bit); when more than one quirk
/// adapter is present the caller passes the adapter's USB topology path so each
/// gets a distinct address.
///
/// Returns `None` when `machine_id` is empty — the caller must skip pinning
/// rather than invent a non-deterministic value (which would collide across
/// cloned images).
pub fn derive_pinned_mac(machine_id: &str, salt: &str) -> Option<MacAddr> {
    if machine_id.trim().is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    if salt.is_empty() {
        hasher.update(machine_id.as_bytes());
    } else {
        hasher.update(machine_id.as_bytes());
        hasher.update(b":");
        hasher.update(salt.as_bytes());
    }
    let digest = hasher.finalize();
    Some(MacAddr([
        0x02, digest[0], digest[1], digest[2], digest[3], digest[4],
    ]))
}

/// The verdict for one network adapter, written to the state file and surfaced
/// on the heartbeat / CLI / setup webapp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterState {
    /// A stable efuse MAC — left untouched.
    Stable,
    /// A randomizer we have pinned; the value lands on the next boot.
    Pinned,
    /// The cross-boot learner suspects randomization but is not certain; the
    /// operator confirms before a pin is written.
    Candidate,
    /// A randomizer we cannot pin yet (e.g. no machine-id, no systemd-udev
    /// link mechanism).
    Deferred,
    /// Pinning is disabled by config.
    Disabled,
}

/// How an adapter's randomizer verdict was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterSource {
    /// Matched the static quirk table.
    Quirk,
    /// Observed to change its MAC across boots.
    Learned,
    /// An operator-supplied explicit MAC override.
    Override,
}

/// Per-adapter record in the state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterVerdict {
    /// Kernel interface name at the time of the verdict (e.g. `wlan0`).
    pub name: String,
    /// `vvvv:pppp` USB id.
    pub vidpid: String,
    /// Stable USB topology path (e.g. `5-1.3`), port-stable across reboots.
    pub usb_path: String,
    pub state: AdapterState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<AdapterSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_mac: Option<MacAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_mac: Option<MacAddr>,
    /// True when the pin was also applied to the live interface (opt-in).
    #[serde(default)]
    pub applied_live: bool,
    /// Path of the `.link` file written for this adapter, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_file: Option<String>,
    /// A short human reason when `state` is `Deferred`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred_reason: Option<String>,
}

/// The cross-boot learner's per-device memory (Layer 2). Keyed by the stable
/// device identity so a churning MAC + churning kernel name still resolves to
/// the same record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnerRecord {
    pub vidpid: String,
    pub usb_path: String,
    pub last_mac: MacAddr,
    pub first_seen: u64,
    pub boot_count: u32,
    pub mac_change_count: u32,
}

/// On-disk schema version for `/etc/ados/mac-pins.state`.
pub const MAC_PINS_STATE_VERSION: u32 = 1;

/// The `/etc/ados/mac-pins.state` document. The installer step + supervisor
/// reconciler write it; the Python heartbeat enricher reads it (so the JSON
/// shape is a cross-language contract — extend additively only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacPinsState {
    pub version: u32,
    #[serde(default)]
    pub adapters: Vec<AdapterVerdict>,
    #[serde(default)]
    pub learner: Vec<LearnerRecord>,
    /// UNIX seconds the state was last written.
    #[serde(default)]
    pub updated_at: u64,
}

impl Default for MacPinsState {
    fn default() -> Self {
        MacPinsState {
            version: MAC_PINS_STATE_VERSION,
            adapters: Vec::new(),
            learner: Vec::new(),
            updated_at: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quirk_table_matches_aic_vendor_family() {
        assert!(is_quirk_randomizer(UsbId {
            vid: 0xa69c,
            pid: 0x8d81
        })
        .is_some());
        assert!(is_quirk_randomizer(UsbId {
            vid: 0xa69c,
            pid: 0x0001
        })
        .is_some());
        assert!(is_quirk_randomizer(UsbId {
            vid: 0x0bda,
            pid: 0xa81a
        })
        .is_none());
        assert!(is_quirk_randomizer(UsbId {
            vid: 0x1234,
            pid: 0x5678
        })
        .is_none());
    }

    #[test]
    fn efuse_allowlist_protects_the_rtl_radio() {
        assert!(is_known_stable_efuse(UsbId {
            vid: 0x0bda,
            pid: 0xa81a
        }));
        assert!(is_known_stable_efuse(UsbId {
            vid: 0x0bda,
            pid: 0x8812
        }));
        assert!(!is_known_stable_efuse(UsbId {
            vid: 0xa69c,
            pid: 0x8d81
        }));
    }

    #[test]
    fn derive_matches_the_proven_rig_value() {
        // Parity lock: this exact machine-id produced 02:c6:75:83:1a:3e on the
        // hardware rig (across a real power-cycle). Changing the derivation must
        // break this test.
        let mac = derive_pinned_mac("03851cd61fc642d781d3f93a00e624cd", "").unwrap();
        assert_eq!(mac.to_string(), "02:c6:75:83:1a:3e");
    }

    #[test]
    fn derived_mac_is_unicast_and_locally_administered() {
        let mac = derive_pinned_mac("any-machine-id", "").unwrap();
        assert!(mac.is_unicast());
        assert!(mac.is_locally_administered());
        assert_eq!(mac.0[0], 0x02);
    }

    #[test]
    fn salt_distinguishes_two_adapters_on_one_box() {
        let a = derive_pinned_mac("mid", "5-1.3").unwrap();
        let b = derive_pinned_mac("mid", "5-1.4").unwrap();
        assert_ne!(a, b);
        let single = derive_pinned_mac("mid", "").unwrap();
        assert_ne!(a, single);
    }

    #[test]
    fn empty_machine_id_skips() {
        assert!(derive_pinned_mac("", "").is_none());
        assert!(derive_pinned_mac("   ", "5-1.3").is_none());
    }

    #[test]
    fn mac_parse_round_trips() {
        let m = MacAddr::parse("02:C6:75:83:1A:3E").unwrap();
        assert_eq!(m.to_string(), "02:c6:75:83:1a:3e");
        assert_eq!(MacAddr::parse("02-c6-75-83-1a-3e").unwrap(), m);
        assert!(MacAddr::parse("not-a-mac").is_none());
        assert!(MacAddr::parse("02:c6:75:83:1a").is_none());
    }

    #[test]
    fn state_file_serializes_to_the_expected_shape() {
        let st = MacPinsState {
            version: MAC_PINS_STATE_VERSION,
            adapters: vec![AdapterVerdict {
                name: "wlan0".into(),
                vidpid: "a69c:8d81".into(),
                usb_path: "5-1.3".into(),
                state: AdapterState::Pinned,
                source: Some(AdapterSource::Quirk),
                pinned_mac: MacAddr::parse("02:c6:75:83:1a:3e"),
                last_seen_mac: MacAddr::parse("88:00:33:77:f3:bd"),
                applied_live: false,
                link_file: Some("/etc/systemd/network/10-ados-mac-wlan0.link".into()),
                deferred_reason: None,
            }],
            learner: Vec::new(),
            updated_at: 1_700_000_000,
        };
        let json = serde_json::to_string(&st).unwrap();
        assert!(json.contains("\"state\":\"pinned\""));
        assert!(json.contains("\"source\":\"quirk\""));
        assert!(json.contains("\"pinned_mac\":\"02:c6:75:83:1a:3e\""));
        let back: MacPinsState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, st);
    }
}
