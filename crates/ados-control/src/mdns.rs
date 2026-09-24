//! Always-on `_ados._tcp` advertisement for this node.
//!
//! # Why the control front owns it
//!
//! The GCS Add-a-Node card browses `_ados._tcp` and probes whatever answers
//! (`/api/lan-pair/discover` → `/api/pairing/info` on each responder). Until
//! this existed, a fresh drone or ground station advertised nothing at boot:
//! the Python `ados-discovery` unit is `OnDemand`, so it starts only when a
//! cloud pairing code is generated, and the static avahi service file the
//! installer drops is ground-station-only. The discovered list was therefore
//! empty on a fresh box, and the operator had to know and type a hostname —
//! an undocumented manual step.
//!
//! The advert belongs to the process that actually answers `/api/pairing/*` on
//! `:8080`. That is this daemon, on every profile, and the record therefore
//! disappears the moment the front stops answering rather than pointing at a
//! port nothing is listening on.
//!
//! # The SRV target is a name that resolves
//!
//! Publishing a record with an arbitrary `server=` does not create a matching
//! A/AAAA record — avahi publishes exactly one resolvable `<hostname>.local`,
//! the system hostname. So the SRV target is
//! [`ados_protocol::reach::mdns_hostname`], the identical name
//! `/api/pairing/info` and the claim response report as `mdns_host`. A host
//! with no usable hostname has no reach to advertise and publishes nothing
//! rather than a name the GCS would store and fail to dial. (`enable_addr_auto`
//! still attaches every interface address, so a browser that prefers the A
//! record reaches the node regardless.)
//!
//! # TXT is refreshed, not poked
//!
//! `paired` and `code` change when an operator claims or releases the node. The
//! refresh task re-reads `pairing.json` on a fixed cadence and re-registers only
//! when a value actually changed, rather than being called from the claim
//! handler: a claim is the write path an operator is waiting on, and it must not
//! be able to block on an mDNS daemon.

use std::path::PathBuf;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceInfo};

use crate::config::PairingConfig;
use crate::pairing_store::PairingDoc;
use crate::state::PairingPaths;

/// The service type the GCS Add-a-Node card browses.
const PAIRING_SERVICE: &str = "_ados._tcp.local.";

/// How often the TXT refresh task re-reads the pairing state. A claim or an
/// unpair is a human-scale event; five seconds is well inside the window an
/// operator would notice and costs one small file read per tick.
const TXT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// A live `_ados._tcp` record for this node. Dropping it unregisters the record
/// and shuts the mDNS daemon down.
pub struct NodeAdvert {
    daemon: ServiceDaemon,
    fullname: String,
    refresh_cancel: ados_protocol::shutdown::Shutdown,
}

impl NodeAdvert {
    /// Unregister and shut down. Also runs on `Drop`, so a panicking daemon
    /// still withdraws the record.
    pub fn shutdown(&self) {
        self.refresh_cancel.trigger();
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

impl Drop for NodeAdvert {
    fn drop(&mut self) {
        self.refresh_cancel.trigger();
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// The identity half of the advert: everything that cannot change without the
/// daemon restarting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertIdentity {
    pub device_id: String,
    pub name: String,
    pub board: String,
    pub version: String,
    pub profile: String,
    pub role: Option<String>,
}

/// The half of the TXT that changes while the daemon runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdvertPairing {
    pub paired: bool,
    /// The pairing code, carried only while unpaired. A paired node publishing
    /// its code would hand a LAN browser a credential it no longer needs.
    pub code: Option<String>,
}

/// The mDNS instance name for this node.
///
/// The system hostname is NOT used: two boards flashed from the same image
/// share a hostname until one is renamed, and a colliding instance name makes
/// the two records fight (mdns-sd appends a disambiguating suffix, so the GCS
/// sees a name that changes between scans). The device id is unique per board
/// by construction.
pub fn instance_name(device_id: &str) -> String {
    let short: String = device_id
        .chars()
        .take(12)
        .collect::<String>()
        .to_lowercase();
    if short.is_empty() {
        "ados-node".to_string()
    } else {
        format!("ados-{short}")
    }
}

/// The TXT record set, as the GCS and the Python `DiscoveryService` both spell
/// it. Pure, so the wire shape is tested without standing up a daemon.
///
/// `code` rides only while unpaired, matching
/// `DiscoveryService._build_txt_records`. `role` is omitted on a profile that
/// has none rather than sent empty, so a consumer reading the key can treat
/// presence as meaning.
pub fn advert_txt(id: &AdvertIdentity, pairing: &AdvertPairing) -> Vec<(String, String)> {
    let mut txt = vec![
        ("device_id".to_string(), id.device_id.clone()),
        ("version".to_string(), id.version.clone()),
        ("board".to_string(), id.board.clone()),
        ("name".to_string(), id.name.clone()),
        ("paired".to_string(), pairing.paired.to_string()),
        ("profile".to_string(), id.profile.clone()),
    ];
    if let Some(role) = id.role.as_ref().filter(|r| !r.is_empty()) {
        txt.push(("role".to_string(), role.clone()));
    }
    if !pairing.paired {
        if let Some(code) = pairing.code.as_ref().filter(|c| !c.is_empty()) {
            txt.push(("code".to_string(), code.clone()));
        }
    }
    txt
}

/// Read the node's fixed identity off the same config + board sidecar
/// `/api/pairing/info` reads, so the browse record and the probe response
/// describe one node.
fn read_identity(paths: &PairingPaths, board_path: &std::path::Path) -> AdvertIdentity {
    let cfg = PairingConfig::load_from(&paths.config);
    let name = if cfg.agent.name.is_empty() {
        "ADOS Agent".to_string()
    } else {
        cfg.agent.name.clone()
    };
    let (profile, role) = crate::profile::current_profile_and_role_at(
        &cfg.agent.profile,
        &paths.profile_conf,
        &paths.mesh_role,
    );
    AdvertIdentity {
        device_id: cfg.agent.device_id.clone(),
        name,
        board: crate::state::board_name(board_path),
        version: crate::state::agent_version(),
        profile,
        role,
    }
}

/// Read the mutable half: whether the node is claimed, and the code it is
/// waiting on while it is not. An unreadable pairing file is advertised as
/// claimed with no code: the node refuses a claim in that state, so inviting
/// one would be false.
fn read_pairing(pairing_json: &std::path::Path) -> AdvertPairing {
    match PairingDoc::read(pairing_json) {
        Ok(doc) if !doc.is_paired() => AdvertPairing {
            code: doc.pairing_code.clone().filter(|c| !c.is_empty()),
            paired: false,
        },
        _ => AdvertPairing {
            code: None,
            paired: true,
        },
    }
}

/// Build the `ServiceInfo` for one publish.
fn build_info(
    server: &str,
    id: &AdvertIdentity,
    pairing: &AdvertPairing,
    port: u16,
) -> Option<ServiceInfo> {
    let txt = advert_txt(id, pairing);
    let txt_refs: Vec<(&str, &str)> = txt.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    // Empty address + `enable_addr_auto` ⇒ advertise on every interface's
    // address, which is what makes the A-record half of the answer usable on a
    // node with an AP, an Ethernet lease and a USB gadget at once.
    match ServiceInfo::new(
        PAIRING_SERVICE,
        &instance_name(&id.device_id),
        server,
        "",
        port,
        &txt_refs[..],
    ) {
        Ok(info) => Some(info.enable_addr_auto()),
        Err(e) => {
            tracing::warn!(error = %e, "mdns_service_info_failed");
            None
        }
    }
}

/// Publish this node on `_ados._tcp` and keep the pairing half of its TXT
/// current for the life of the process.
///
/// Returns `None` — no advert — when the host has no resolvable hostname to
/// name as the SRV target, or when the mDNS daemon cannot start. Neither is
/// fatal: Add-a-Node by IP always works, and advertising a name that resolves
/// nowhere is worse than advertising nothing.
pub fn advertise(paths: &PairingPaths, board_path: PathBuf, port: u16) -> Option<NodeAdvert> {
    let Some(hostname) = ados_protocol::reach::mdns_hostname() else {
        tracing::warn!(
            "mdns_advert_skipped_no_resolvable_hostname: this host has no hostname a \
             GCS could dial, so no `_ados._tcp` record is published. Set a hostname \
             to appear in Add-a-Node discovery; pairing by IP is unaffected."
        );
        return None;
    };
    // mDNS wants the SRV target fully qualified.
    let server = format!("{}.", hostname.trim_end_matches('.'));

    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "mdns_daemon_failed");
            return None;
        }
    };

    let id = read_identity(paths, &board_path);
    let mut pairing = read_pairing(&paths.pairing_json);
    let info = match build_info(&server, &id, &pairing, port) {
        Some(i) => i,
        None => {
            let _ = daemon.shutdown();
            return None;
        }
    };
    let fullname = info.get_fullname().to_string();
    if let Err(e) = daemon.register(info) {
        tracing::warn!(error = %e, "mdns_register_failed");
        let _ = daemon.shutdown();
        return None;
    }
    tracing::info!(
        service = PAIRING_SERVICE,
        instance = %fullname,
        server = %server,
        port,
        profile = %id.profile,
        paired = pairing.paired,
        "mdns_published"
    );

    // Re-announce when the pairing half changes. Re-registering the same
    // fullname replaces the record in place (mdns-sd documents this as the
    // update path), so the GCS sees `paired=true` and the code disappear
    // without the record ever going away.
    let refresh_cancel = ados_protocol::shutdown::Shutdown::new();
    {
        let cancel = refresh_cancel.clone();
        let daemon = daemon.clone();
        let pairing_json = paths.pairing_json.clone();
        let id = id.clone();
        let server = server.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.wait() => return,
                    _ = tokio::time::sleep(TXT_REFRESH_INTERVAL) => {}
                }
                let next = read_pairing(&pairing_json);
                if next == pairing {
                    continue;
                }
                // The published half only advances once the record is actually
                // re-registered, so a failed refresh is retried next tick rather
                // than leaving a stale paired flag or pairing code on the LAN.
                let Some(info) = build_info(&server, &id, &next, port) else {
                    continue;
                };
                match daemon.register(info) {
                    Ok(()) => {
                        pairing = next;
                        tracing::info!(paired = pairing.paired, "mdns_txt_refreshed");
                    }
                    Err(e) => tracing::warn!(error = %e, "mdns_txt_refresh_failed"),
                }
            }
        });
    }

    Some(NodeAdvert {
        daemon,
        fullname,
        refresh_cancel,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> AdvertIdentity {
        AdvertIdentity {
            device_id: "abcdef0123456789".to_string(),
            name: "my-drone".to_string(),
            board: "rock-5c-lite".to_string(),
            version: "0.1.0".to_string(),
            profile: "drone".to_string(),
            role: None,
        }
    }

    fn txt_map(txt: &[(String, String)]) -> std::collections::HashMap<&str, &str> {
        txt.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
    }

    #[test]
    fn an_unpaired_node_advertises_its_code_so_the_six_character_path_can_match() {
        let txt = advert_txt(
            &identity(),
            &AdvertPairing {
                paired: false,
                code: Some("K7M2QX".to_string()),
            },
        );
        let map = txt_map(&txt);
        assert_eq!(map.get("paired"), Some(&"false"));
        assert_eq!(map.get("code"), Some(&"K7M2QX"));
        assert_eq!(map.get("device_id"), Some(&"abcdef0123456789"));
        assert_eq!(map.get("profile"), Some(&"drone"));
        assert_eq!(map.get("board"), Some(&"rock-5c-lite"));
    }

    #[test]
    fn a_paired_node_stops_advertising_its_code() {
        // The code is a claim credential. Once the node is claimed, publishing
        // it to every browser on the LAN buys nothing and hands out a secret.
        let txt = advert_txt(
            &identity(),
            &AdvertPairing {
                paired: true,
                code: Some("K7M2QX".to_string()),
            },
        );
        let map = txt_map(&txt);
        assert_eq!(map.get("paired"), Some(&"true"));
        assert!(!map.contains_key("code"));
    }

    #[test]
    fn role_is_omitted_rather_than_empty_on_a_profile_that_has_none() {
        let plain = advert_txt(&identity(), &AdvertPairing::default());
        assert!(!txt_map(&plain).contains_key("role"));

        let mut gs = identity();
        gs.profile = "ground-station".to_string();
        gs.role = Some("receiver".to_string());
        let with_role = advert_txt(&gs, &AdvertPairing::default());
        assert_eq!(txt_map(&with_role).get("role"), Some(&"receiver"));
    }

    #[test]
    fn the_instance_name_is_derived_from_the_device_id_not_the_hostname() {
        // Two boards flashed from one image share a hostname; the device id is
        // what keeps their records distinct.
        assert_eq!(instance_name("ABCDEF0123456789"), "ados-abcdef012345");
        assert_eq!(instance_name("ab12"), "ados-ab12");
        assert_eq!(instance_name(""), "ados-node");
    }
}
