//! The `mdns.advertise` and `mdns.browse` plugin methods: DNS-SD on the LAN,
//! done by the host on a plugin's behalf.
//!
//! A sandboxed plugin cannot run an mDNS responder itself. Every plugin unit
//! carries `SocketBindDeny=any`, and systemd's bind filter matches every port
//! under `any`, the ephemeral port 0 included, so an in-process daemon fails at
//! its first `bind` (the signal socket) before it could reach the multicast
//! port. The host is not sandboxed and publishes the record with the same
//! library and the same SRV-target rule core's own `_ados._tcp` advert uses:
//! the system hostname as [`crate::reach::mdns_hostname`] resolves it, the one
//! name avahi answers for.
//!
//! An advert is owned by the plugin connection that made it and is withdrawn
//! when that connection ends, so a record never outlives the process serving
//! the port it names. Its port must be one of the plugin's declared
//! `listen_ports`: a plugin can only advertise what the sandbox lets it serve.
//! The service types core publishes itself ([`RESERVED_SERVICE_TYPES`]) are
//! refused, so a plugin cannot impersonate a node's pairing record or a ground
//! station's receiver. Browsing any well-formed type is allowed.
//!
//! The types are the wire shape: the host decodes the request and serializes
//! the reply with them, and the SDK does the reverse, so the two cannot drift.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The advertise method's wire name.
pub const ADVERTISE_METHOD: &str = "mdns.advertise";
/// The capability `mdns.advertise` is gated on: advertising a port is offering
/// it to the network, which is what `network.listen` grants.
pub const ADVERTISE_CAPABILITY: &str = "network.listen";
/// The browse method's wire name.
pub const BROWSE_METHOD: &str = "mdns.browse";
/// The capability `mdns.browse` is gated on: a browse finds peers to connect
/// out to.
pub const BROWSE_CAPABILITY: &str = "network.outbound";

/// The service types core publishes. A plugin may browse them but never
/// advertise one: `_ados._tcp` is every node's pairing record (the control
/// front on every profile, the static avahi file on a ground station's access
/// point) and `_ados-receiver._tcp` is the ground-station mesh receiver.
pub const RESERVED_SERVICE_TYPES: &[&str] = &["_ados._tcp", "_ados-receiver._tcp"];

/// The browse window when the request names none.
pub const BROWSE_DEFAULT_TIMEOUT_MS: u32 = 3_000;
/// The longest browse window a request may ask for.
pub const BROWSE_MAX_TIMEOUT_MS: u32 = 5_000;
/// The shortest browse window: one mDNS query round trip.
pub const BROWSE_MIN_TIMEOUT_MS: u32 = 100;
/// The most services one browse returns.
pub const BROWSE_MAX_RESULTS: usize = 64;

/// The most TXT entries one advert may carry.
pub const TXT_MAX_ENTRIES: usize = 16;
/// The longest TXT key.
pub const TXT_MAX_KEY_LEN: usize = 32;
/// The longest TXT value, in bytes.
pub const TXT_MAX_VALUE_LEN: usize = 200;

/// An `mdns.advertise` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvertiseRequest {
    /// The DNS-SD service type, `_<name>._tcp` (a trailing `.local.` is
    /// accepted). TCP only: the ports a plugin may serve are TCP ports.
    pub service_type: String,
    /// The port the service answers on; one of the plugin's declared
    /// `listen_ports` on this node's profile.
    pub port: u16,
    /// The TXT record, at most [`TXT_MAX_ENTRIES`] entries.
    #[serde(default)]
    pub txt: BTreeMap<String, String>,
}

/// The record the host published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advertised {
    /// The full service instance name
    /// (`<instance>.<service type>.local.`).
    pub fullname: String,
    /// The SRV target: the system hostname avahi answers for.
    pub hostname: String,
    pub port: u16,
}

/// An `mdns.browse` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowseRequest {
    /// The DNS-SD service type to browse, `_<name>._tcp` or `_<name>._udp`.
    pub service_type: String,
    /// How long to collect answers, clamped to
    /// [`BROWSE_MIN_TIMEOUT_MS`]..=[`BROWSE_MAX_TIMEOUT_MS`];
    /// [`BROWSE_DEFAULT_TIMEOUT_MS`] when absent.
    #[serde(default)]
    pub timeout_ms: Option<u32>,
}

/// What one browse found.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BrowseReply {
    /// Every instance resolved inside the window, in the order first seen, at
    /// most [`BROWSE_MAX_RESULTS`].
    pub services: Vec<DiscoveredService>,
}

/// One resolved service instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredService {
    /// The full service instance name.
    pub fullname: String,
    /// The SRV target, without the trailing dot.
    pub hostname: String,
    pub port: u16,
    /// The addresses the instance answered with, IPv4 first, each group sorted.
    pub addresses: Vec<String>,
    pub txt: BTreeMap<String, String>,
}

impl BrowseRequest {
    /// The browse window this request asks for, clamped to the allowed range.
    pub fn timeout_ms(&self) -> u32 {
        self.timeout_ms
            .unwrap_or(BROWSE_DEFAULT_TIMEOUT_MS)
            .clamp(BROWSE_MIN_TIMEOUT_MS, BROWSE_MAX_TIMEOUT_MS)
    }
}

/// The canonical `_<name>._<tcp|udp>` form of a service type (a trailing
/// `.local.` or `.local` is stripped), or why it is not one.
///
/// The name follows RFC 6335: 1-15 characters of lowercase letters, digits and
/// hyphens, at least one letter, no leading, trailing or doubled hyphen.
/// Uppercase is folded, since DNS names compare case-insensitively.
pub fn normalize_service_type(raw: &str) -> Result<String, String> {
    let lowered = raw.trim().to_ascii_lowercase();
    let bare = lowered
        .strip_suffix(".local.")
        .or_else(|| lowered.strip_suffix(".local"))
        .unwrap_or(&lowered);
    let (name, proto) = bare
        .rsplit_once('.')
        .ok_or_else(|| format!("service type {raw:?} is not _<name>._tcp or _<name>._udp"))?;
    if proto != "_tcp" && proto != "_udp" {
        return Err(format!("service type {raw:?} must end in ._tcp or ._udp"));
    }
    let label = name
        .strip_prefix('_')
        .ok_or_else(|| format!("service type {raw:?} must start with an underscore"))?;
    let valid = (1..=15).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && label.bytes().any(|b| b.is_ascii_lowercase())
        && !label.starts_with('-')
        && !label.ends_with('-')
        && !label.contains("--");
    if !valid {
        return Err(format!(
            "service type {raw:?}: the name must be 1-15 letters, digits and single inner hyphens"
        ));
    }
    Ok(format!("_{label}.{proto}"))
}

/// The service type a plugin may advertise, canonical: TCP, well formed, and
/// not one core publishes.
pub fn advertisable_service_type(raw: &str) -> Result<String, String> {
    let ty = normalize_service_type(raw)?;
    if !ty.ends_with("._tcp") {
        return Err(format!(
            "service type {ty} is not TCP; a plugin serves only the TCP ports it declares"
        ));
    }
    if RESERVED_SERVICE_TYPES.contains(&ty.as_str()) {
        return Err(format!(
            "service type {ty} is published by the agent itself"
        ));
    }
    Ok(ty)
}

/// Check a TXT record against the size and key rules: at most
/// [`TXT_MAX_ENTRIES`] entries, keys of 1..=[`TXT_MAX_KEY_LEN`] printable ASCII
/// characters without `=`, values of at most [`TXT_MAX_VALUE_LEN`] bytes.
pub fn validate_txt(txt: &BTreeMap<String, String>) -> Result<(), String> {
    if txt.len() > TXT_MAX_ENTRIES {
        return Err(format!(
            "txt carries {} entries; at most {TXT_MAX_ENTRIES}",
            txt.len()
        ));
    }
    for (key, value) in txt {
        let key_ok = (1..=TXT_MAX_KEY_LEN).contains(&key.len())
            && key.bytes().all(|b| (0x21..=0x7e).contains(&b) && b != b'=');
        if !key_ok {
            return Err(format!(
                "txt key {key:?} must be 1-{TXT_MAX_KEY_LEN} printable ASCII characters without '='"
            ));
        }
        if value.len() > TXT_MAX_VALUE_LEN {
            return Err(format!(
                "txt value for {key:?} is {} bytes; at most {TXT_MAX_VALUE_LEN}",
                value.len()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_types_normalize_to_the_bare_form() {
        for raw in [
            "_ados-compute._tcp",
            "_ados-compute._tcp.local",
            "_ados-compute._tcp.local.",
            " _ADOS-Compute._TCP.local. ",
        ] {
            assert_eq!(
                normalize_service_type(raw).as_deref(),
                Ok("_ados-compute._tcp"),
                "{raw}"
            );
        }
        assert_eq!(
            normalize_service_type("_rtsp._udp").as_deref(),
            Ok("_rtsp._udp")
        );
    }

    #[test]
    fn malformed_service_types_are_refused() {
        for raw in [
            "",
            "ados._tcp",
            "_ados._sctp",
            "_._tcp",
            "_-ados._tcp",
            "_ados-._tcp",
            "_ad--os._tcp",
            "_1234._tcp",
            "_sixteen-chars-xx._tcp",
            "_ados_x._tcp",
            "_ados.x._tcp",
        ] {
            assert!(normalize_service_type(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn a_plugin_may_not_advertise_a_core_type_or_a_udp_one() {
        assert!(advertisable_service_type("_ados._tcp.local.").is_err());
        assert!(advertisable_service_type("_ADOS-receiver._tcp").is_err());
        assert!(advertisable_service_type("_ados-compute._udp").is_err());
        assert_eq!(
            advertisable_service_type("_ados-compute._tcp").as_deref(),
            Ok("_ados-compute._tcp")
        );
    }

    #[test]
    fn txt_records_are_bounded() {
        let one = |k: &str, v: &str| BTreeMap::from([(k.to_string(), v.to_string())]);
        assert!(validate_txt(&one("deviceId", "compute-8270beb76258")).is_ok());
        assert!(validate_txt(&one("", "x")).is_err());
        assert!(validate_txt(&one("a=b", "x")).is_err());
        assert!(validate_txt(&one("sp ace", "x")).is_err());
        assert!(validate_txt(&one(&"k".repeat(TXT_MAX_KEY_LEN + 1), "x")).is_err());
        assert!(validate_txt(&one("k", &"v".repeat(TXT_MAX_VALUE_LEN + 1))).is_err());
        let many: BTreeMap<String, String> = (0..=TXT_MAX_ENTRIES)
            .map(|i| (format!("k{i}"), String::new()))
            .collect();
        assert!(validate_txt(&many).is_err());
    }

    #[test]
    fn a_browse_window_is_clamped() {
        let req = |t: Option<u32>| BrowseRequest {
            service_type: "_x._tcp".into(),
            timeout_ms: t,
        };
        assert_eq!(req(None).timeout_ms(), BROWSE_DEFAULT_TIMEOUT_MS);
        assert_eq!(req(Some(0)).timeout_ms(), BROWSE_MIN_TIMEOUT_MS);
        assert_eq!(req(Some(60_000)).timeout_ms(), BROWSE_MAX_TIMEOUT_MS);
        assert_eq!(req(Some(1_500)).timeout_ms(), 1_500);
    }
}
