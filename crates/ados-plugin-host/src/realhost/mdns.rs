//! `mdns.advertise` and `mdns.browse`: DNS-SD done by the host for a plugin.
//!
//! A plugin unit carries `SocketBindDeny=any`, and systemd's bind filter
//! matches the ephemeral port 0 under `any` too, so an in-process responder in
//! a plugin fails at its very first bind. The host is not sandboxed, and it
//! publishes the way core publishes `_ados._tcp` (`ados-control`'s advert):
//! the `mdns-sd` responder, every interface's address attached, and the SRV
//! target the system hostname as [`ados_protocol::reach::mdns_hostname`]
//! resolves it, the one name avahi answers for. A node with no such hostname
//! advertises nothing rather than a name that resolves nowhere.
//!
//! One long-lived responder carries every plugin's records. A record belongs
//! to the connection that published it and is withdrawn when that connection
//! ends ([`RealHost::release_session`]), so it never outlives the process that
//! serves its port. Its port must be a listen port the plugin declares for a
//! service that runs on this node's profile, which is exactly the set of ports
//! the plugin's units may bind.
//!
//! A browse runs on a responder of its own: `mdns-sd` keeps one listener per
//! service type, so two concurrent browses of one type on a shared responder
//! would steal each other's answers.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;
use std::path::Path;

use ados_protocol::plugin_mdns::{
    advertisable_service_type, normalize_service_type, validate_txt, AdvertiseRequest, Advertised,
    BrowseReply, BrowseRequest, DiscoveredService, ADVERTISE_METHOD, BROWSE_MAX_RESULTS,
    BROWSE_METHOD,
};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use super::*;

/// The longest DNS-SD instance label.
const INSTANCE_MAX_LEN: usize = 63;

/// The connection that published a record.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordOwner {
    plugin_id: String,
    session: u64,
}

/// The host's responder and the records plugins published through it.
#[derive(Default)]
pub(super) struct MdnsRecords {
    /// Started on the first advert and kept for the process. A responder that
    /// fails to start is retried on the next advert, never given up on.
    daemon: Mutex<Option<ServiceDaemon>>,
    /// Every live record, by full service name.
    owners: Mutex<HashMap<String, RecordOwner>>,
}

impl MdnsRecords {
    fn daemon(&self) -> Result<ServiceDaemon, String> {
        let mut slot = self.daemon.lock().expect("mdns daemon mutex poisoned");
        if let Some(daemon) = slot.as_ref() {
            return Ok(daemon.clone());
        }
        let daemon =
            ServiceDaemon::new().map_err(|e| format!("mdns responder did not start: {e}"))?;
        *slot = Some(daemon.clone());
        Ok(daemon)
    }

    /// Publish `info` for `owner`. Re-publishing a name the plugin already
    /// holds replaces the record in place (a TXT update) and moves it to the
    /// newer connection; a name another plugin holds is refused.
    fn publish(&self, info: ServiceInfo, owner: RecordOwner) -> Result<String, String> {
        let fullname = info.get_fullname().to_string();
        let mut owners = self.owners.lock().expect("mdns owners mutex poisoned");
        claim(&owners, &fullname, &owner)?;
        self.daemon()?
            .register(info)
            .map_err(|e| format!("mdns register failed: {e}"))?;
        owners.insert(fullname.clone(), owner);
        Ok(fullname)
    }

    /// Withdraw every record `session` of `plugin_id` published.
    pub(super) fn release(&self, plugin_id: &str, session: u64) {
        let names = {
            let mut owners = self.owners.lock().expect("mdns owners mutex poisoned");
            release_owned(&mut owners, plugin_id, session)
        };
        if names.is_empty() {
            return;
        }
        let Some(daemon) = self
            .daemon
            .lock()
            .expect("mdns daemon mutex poisoned")
            .clone()
        else {
            return;
        };
        for name in names {
            match daemon.unregister(&name) {
                Ok(_) => tracing::info!(plugin_id, record = %name, "mdns record withdrawn"),
                Err(e) => {
                    tracing::warn!(plugin_id, record = %name, error = %e, "mdns unregister failed")
                }
            }
        }
    }
}

/// Whether `owner` may publish `fullname`: free, or already this plugin's.
fn claim(
    owners: &HashMap<String, RecordOwner>,
    fullname: &str,
    owner: &RecordOwner,
) -> Result<(), String> {
    match owners.get(fullname) {
        Some(held) if held.plugin_id != owner.plugin_id => Err(format!(
            "mdns record {fullname} is published by another plugin"
        )),
        _ => Ok(()),
    }
}

/// Remove and return the records `session` of `plugin_id` owns. A record a
/// newer connection of the same plugin re-published is left alone.
fn release_owned(
    owners: &mut HashMap<String, RecordOwner>,
    plugin_id: &str,
    session: u64,
) -> Vec<String> {
    let names: Vec<String> = owners
        .iter()
        .filter(|(_, o)| o.plugin_id == plugin_id && o.session == session)
        .map(|(name, _)| name.clone())
        .collect();
    for name in &names {
        owners.remove(name);
    }
    names
}

/// The instance label a plugin's record is published under: the plugin id and
/// this node's device id, so the same plugin on two nodes, and two plugins on
/// one node, never publish one name. A node with no device id yet uses its
/// hostname's first label.
fn instance_name(plugin_id: &str, device_id: &str, hostname: &str) -> String {
    let device = device_id.trim();
    let node: String = if device.is_empty() {
        hostname
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    } else {
        device
            .chars()
            .take(12)
            .collect::<String>()
            .to_ascii_lowercase()
    };
    let suffix = format!("-{node}");
    let plugin: String = plugin_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(INSTANCE_MAX_LEN.saturating_sub(suffix.len()))
        .collect();
    format!("{plugin}{suffix}")
}

/// The TCP ports the manifest at `manifest` declares for the services that run
/// on a `profile` node. Empty when there is no manifest or it does not parse.
fn declared_listen_ports(manifest: &Path, profile: &str) -> BTreeSet<u16> {
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return BTreeSet::new();
    };
    let Ok(manifest) = crate::manifest::PluginManifest::from_yaml_text(&text) else {
        return BTreeSet::new();
    };
    crate::services::declared_services(&manifest)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.applies_to(profile))
        .flat_map(|s| s.listen_ports)
        .collect()
}

/// One resolved instance in its wire form.
fn discovered(info: &ServiceInfo) -> DiscoveredService {
    let (mut v4, mut v6): (Vec<IpAddr>, Vec<IpAddr>) =
        info.get_addresses().iter().partition(|a| a.is_ipv4());
    v4.sort();
    v6.sort();
    DiscoveredService {
        fullname: info.get_fullname().to_string(),
        hostname: info.get_hostname().trim_end_matches('.').to_string(),
        port: info.get_port(),
        addresses: v4.iter().chain(&v6).map(IpAddr::to_string).collect(),
        txt: info
            .get_properties()
            .iter()
            .map(|p| (p.key().to_string(), p.val_str().to_string()))
            .collect::<BTreeMap<_, _>>(),
    }
}

/// Collect every instance of `ty_domain` resolved within `window`, on a
/// responder of its own.
async fn browse(ty_domain: &str, window: Duration) -> Result<Vec<DiscoveredService>, String> {
    let daemon = ServiceDaemon::new().map_err(|e| format!("mdns responder did not start: {e}"))?;
    let rx = match daemon.browse(ty_domain) {
        Ok(rx) => rx,
        Err(e) => {
            let _ = daemon.shutdown();
            return Err(format!("mdns browse failed: {e}"));
        }
    };
    let mut found: Vec<DiscoveredService> = Vec::new();
    let _ = tokio::time::timeout(window, async {
        while let Ok(event) = rx.recv_async().await {
            let ServiceEvent::ServiceResolved(info) = event else {
                continue;
            };
            let service = discovered(&info);
            // A later answer for the same instance (an address or TXT change)
            // replaces the earlier one in place.
            match found.iter().position(|s| s.fullname == service.fullname) {
                Some(i) => found[i] = service,
                None if found.len() < BROWSE_MAX_RESULTS => found.push(service),
                None => {}
            }
        }
    })
    .await;
    let _ = daemon.shutdown();
    Ok(found)
}

/// Decode a request's arguments into its wire type.
fn decode<T: serde::de::DeserializeOwned>(args: &Value, method: &str) -> Result<T, HostError> {
    rmpv::ext::from_value(args.clone())
        .map_err(|e| HostError::Rpc(format!("{method} arguments are invalid: {e}")))
}

impl RealHost {
    /// `mdns.advertise`: publish one of the plugin's declared listen ports.
    pub(super) async fn advertise_mdns(
        &self,
        plugin_id: &str,
        session: u64,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let req: AdvertiseRequest = decode(args, ADVERTISE_METHOD)?;
        let service_type = advertisable_service_type(&req.service_type).map_err(HostError::Rpc)?;
        validate_txt(&req.txt).map_err(HostError::Rpc)?;

        // The manifest read and the hostname probe touch disk (and, off
        // Linux, spawn `hostname`), so they run on the blocking pool.
        let lookup = self.plugin_runtime_lookup.clone();
        let sources = self.node_info_sources.clone();
        let id = plugin_id.to_string();
        let (ports, hostname) = tokio::task::spawn_blocking(move || {
            let profile = ados_config::node_profile_at(&sources.config_yaml, &sources.profile_conf);
            let ports = lookup
                .and_then(|lookup| lookup(&id))
                .map(|(dir, _)| declared_listen_ports(&dir.join("manifest.yaml"), &profile))
                .unwrap_or_default();
            (ports, ados_protocol::reach::mdns_hostname())
        })
        .await
        .map_err(|e| HostError::Rpc(format!("{ADVERTISE_METHOD} failed: {e}")))?;

        if !ports.contains(&req.port) {
            return Err(HostError::Rpc(format!(
                "port {} is not a listen port this plugin declares for a service on this node",
                req.port
            )));
        }
        let Some(hostname) = hostname else {
            return Err(HostError::Rpc(
                "no_resolvable_hostname: this node has no hostname another machine could \
                 resolve, so nothing is advertised"
                    .to_string(),
            ));
        };

        let instance = instance_name(plugin_id, &self.agent_id_for(plugin_id), &hostname);
        let properties: Vec<(&str, &str)> = req
            .txt
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let info = ServiceInfo::new(
            &format!("{service_type}.local."),
            &instance,
            &format!("{}.", hostname.trim_end_matches('.')),
            "",
            req.port,
            &properties[..],
        )
        .map_err(|e| HostError::Rpc(format!("mdns record is invalid: {e}")))?
        .enable_addr_auto();
        let fullname = self
            .mdns
            .publish(
                info,
                RecordOwner {
                    plugin_id: plugin_id.to_string(),
                    session,
                },
            )
            .map_err(HostError::Rpc)?;
        tracing::info!(
            plugin_id,
            record = %fullname,
            server = %hostname,
            port = req.port,
            "mdns record published"
        );
        serialize_reply(&Advertised {
            fullname,
            hostname,
            port: req.port,
        })
    }

    /// `mdns.browse`: every instance of one service type answering within the
    /// request's window.
    pub(super) async fn browse_mdns(&self, args: &Value) -> Result<HostResult, HostError> {
        let req: BrowseRequest = decode(args, BROWSE_METHOD)?;
        let service_type = normalize_service_type(&req.service_type).map_err(HostError::Rpc)?;
        let window = Duration::from_millis(u64::from(req.timeout_ms()));
        let services = browse(&format!("{service_type}.local."), window)
            .await
            .map_err(HostError::Rpc)?;
        serialize_reply(&BrowseReply { services })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(plugin_id: &str, session: u64) -> RecordOwner {
        RecordOwner {
            plugin_id: plugin_id.to_string(),
            session,
        }
    }

    #[test]
    fn a_record_leaves_with_the_connection_that_published_it() {
        let mut owners = HashMap::new();
        owners.insert("a._x._tcp.local.".to_string(), owner("com.x.p", 1));
        owners.insert("b._x._tcp.local.".to_string(), owner("com.x.p", 2));
        owners.insert("c._x._tcp.local.".to_string(), owner("com.y.q", 1));

        // Session 1 of com.x.p ending takes only its own record: not the one
        // its service's concurrent connection published, not another plugin's
        // record on a session with the same number.
        assert_eq!(
            release_owned(&mut owners, "com.x.p", 1),
            vec!["a._x._tcp.local."]
        );
        assert_eq!(owners.len(), 2);
        assert!(release_owned(&mut owners, "com.x.p", 1).is_empty());
    }

    #[test]
    fn a_plugin_may_republish_its_own_name_but_not_another_plugins() {
        let mut owners = HashMap::new();
        owners.insert("a._x._tcp.local.".to_string(), owner("com.x.p", 1));
        // The same plugin on a newer connection (a restarted service) takes it over.
        assert!(claim(&owners, "a._x._tcp.local.", &owner("com.x.p", 7)).is_ok());
        assert!(claim(&owners, "a._x._tcp.local.", &owner("com.y.q", 7)).is_err());
        assert!(claim(&owners, "free._x._tcp.local.", &owner("com.y.q", 7)).is_ok());
    }

    #[test]
    fn instance_names_are_per_plugin_per_node_and_fit_a_label() {
        assert_eq!(
            instance_name("com.altnautica.world-engine", "5A89C1FEABE9", "ws.local"),
            "com-altnautica-world-engine-5a89c1feabe9"
        );
        // No device id yet: the hostname's first label names the node.
        assert_eq!(
            instance_name("com.altnautica.world-engine", " ", "skynode.local"),
            "com-altnautica-world-engine-skynode"
        );
        let long = instance_name(&"p".repeat(80), "0011aabbccdd", "h.local");
        assert_eq!(long.len(), INSTANCE_MAX_LEN);
        assert!(long.ends_with("-0011aabbccdd"));
    }

    #[test]
    fn listen_ports_come_from_the_services_that_run_on_this_profile() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("manifest.yaml");
        std::fs::write(
            &manifest,
            "id: com.example.node\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\n\
             agent:\n  entrypoint: agent/py/x.py\n  permissions: [network.outbound, network.listen]\n  \
             contributes:\n    services:\n      - name: node\n        command: bin/node\n        \
             listen_ports: [8092]\n        profiles: [workstation]\n      - name: edge\n        \
             command: bin/edge\n        listen_ports: [9100]\n",
        )
        .unwrap();
        assert_eq!(
            declared_listen_ports(&manifest, "workstation"),
            BTreeSet::from([8092, 9100])
        );
        assert_eq!(
            declared_listen_ports(&manifest, "drone"),
            BTreeSet::from([9100])
        );
        assert!(declared_listen_ports(&dir.path().join("absent.yaml"), "drone").is_empty());
    }
}
