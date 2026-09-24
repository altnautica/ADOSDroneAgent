//! Node-to-node credentials: the scoped key a workstation issues so a drone (or
//! a ground station relaying for one) may use that workstation's lanes.
//!
//! Every node mints its own pairing key, so a drone's key means nothing to a
//! workstation, and the drone's key must never be handed to one either (it is
//! full command authority over the aircraft, and any LAN host can advertise
//! itself as a workstation). Instead the ground station, which holds the owner
//! key of both nodes, asks the workstation to issue a credential scoped to the
//! lanes a drone uses and installs it on the drone. The drone then presents it
//! in [`NODE_CREDENTIAL_HEADER`] on every drone-to-workstation lane, and the
//! workstation admits it only for the lanes it was issued for.
//!
//! This module holds what both ends share: the header name, the lane names,
//! and the drone-side store of installed credentials
//! ([`WorkstationCredentials`], persisted 0600 at
//! [`WORKSTATION_CREDENTIALS_PATH`]). Issuing and verifying live with the
//! workstation.

use std::borrow::Cow;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The header a node presents its workstation-issued credential in. Distinct
/// from `X-ADOS-Key`, which always carries an owner's pairing key.
pub const NODE_CREDENTIAL_HEADER: &str = "x-ados-node-credential";

/// Where a node keeps the credentials workstations issued it.
pub const WORKSTATION_CREDENTIALS_PATH: &str = "/etc/ados/workstation-credentials.json";

/// Environment override for [`WORKSTATION_CREDENTIALS_PATH`] (bench and dev
/// installs that keep `/etc/ados` elsewhere).
pub const WORKSTATION_CREDENTIALS_ENV: &str = "ADOS_WORKSTATION_CREDENTIALS";

/// One drone-to-workstation lane a credential can be scoped to, by its wire
/// name (`atlas.ingest`, `offload.stream`, ...). The lane vocabulary belongs to
/// the service that serves the lanes; this type only guarantees a name is well
/// formed: 1-64 characters of `[a-z0-9._-]`. Anything else is refused when it
/// is parsed or deserialized, so a stored or requested lane can never smuggle
/// arbitrary text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct NodeLane(Cow<'static, str>);

/// Longest lane name.
pub const NODE_LANE_MAX_LEN: usize = 64;

const fn is_lane_name(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > NODE_LANE_MAX_LEN {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_' || b == b'-') {
            return false;
        }
        i += 1;
    }
    true
}

impl NodeLane {
    /// A lane from a name known at compile time, for a service's own lane
    /// constants. Panics (at compile time, in a `const`) on a malformed name.
    pub const fn from_static(name: &'static str) -> Self {
        assert!(
            is_lane_name(name),
            "node lane names are [a-z0-9._-]{{1,64}}"
        );
        NodeLane(Cow::Borrowed(name))
    }

    /// Parse a wire name; `None` when it is not a well-formed lane name.
    pub fn parse(s: &str) -> Option<Self> {
        is_lane_name(s).then(|| NodeLane(Cow::Owned(s.to_string())))
    }

    /// The wire name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for NodeLane {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        NodeLane::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "node lane {raw:?} must be 1-{NODE_LANE_MAX_LEN} characters of [a-z0-9._-]"
            ))
        })
    }
}

/// One credential a workstation issued this node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledCredential {
    /// The issuing workstation's compute node id: the id it advertises over
    /// mDNS (`deviceId` TXT) and names in its mint reply, so the lanes can pick
    /// the credential for the node they resolved.
    pub workstation_node_id: String,
    /// The secret token presented in [`NODE_CREDENTIAL_HEADER`].
    pub credential: String,
    /// The lanes it was issued for.
    pub lanes: Vec<NodeLane>,
    /// Epoch milliseconds it was installed here.
    pub installed_at_ms: i64,
}

/// Every credential installed on this node, one per issuing workstation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkstationCredentials {
    #[serde(default)]
    pub workstations: Vec<InstalledCredential>,
}

impl WorkstationCredentials {
    /// The store path: the [`WORKSTATION_CREDENTIALS_ENV`] override, else
    /// [`WORKSTATION_CREDENTIALS_PATH`].
    pub fn default_path() -> PathBuf {
        std::env::var_os(WORKSTATION_CREDENTIALS_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(WORKSTATION_CREDENTIALS_PATH))
    }

    /// Load the store. An absent file is an empty store (nothing installed).
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Load for a lane that only needs to present a credential: any read or
    /// parse failure reads as nothing installed (the workstation then refuses
    /// the lane, which is the honest outcome), and is logged.
    pub fn load_or_empty(path: &Path) -> Self {
        Self::load(path).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "workstation credentials unreadable");
            Self::default()
        })
    }

    /// The credential to present to a workstation. A known node id picks its
    /// own credential and nothing else, so a credential is never offered to a
    /// node it was not issued by. An unknown id (a pinned address carries none)
    /// picks the sole installed credential, and none when there are several.
    pub fn for_node(&self, workstation_node_id: Option<&str>) -> Option<&InstalledCredential> {
        match workstation_node_id.filter(|id| !id.is_empty()) {
            Some(id) => self
                .workstations
                .iter()
                .find(|c| c.workstation_node_id == id),
            None if self.workstations.len() == 1 => self.workstations.first(),
            None => None,
        }
    }

    /// Insert or replace the credential for its workstation.
    pub fn upsert(&mut self, credential: InstalledCredential) {
        self.workstations
            .retain(|c| c.workstation_node_id != credential.workstation_node_id);
        self.workstations.push(credential);
    }

    /// Persist atomically (temp sibling, fsync, rename) and owner-only: the file
    /// carries secrets.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

/// The credential string this node should present to `workstation_node_id`,
/// read from the default store. The one-call form the lanes use.
pub fn credential_for(workstation_node_id: Option<&str>) -> Option<String> {
    WorkstationCredentials::load_or_empty(&WorkstationCredentials::default_path())
        .for_node(workstation_node_id)
        .map(|c| c.credential.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(node: &str, secret: &str) -> InstalledCredential {
        InstalledCredential {
            workstation_node_id: node.into(),
            credential: secret.into(),
            lanes: vec![NodeLane::from_static("atlas.ingest")],
            installed_at_ms: 1,
        }
    }

    #[test]
    fn a_lane_is_its_wire_name_and_nothing_malformed_parses() {
        let lane = NodeLane::parse("offload.stream").unwrap();
        assert_eq!(lane.as_str(), "offload.stream");
        assert_eq!(lane, NodeLane::from_static("offload.stream"));
        assert_eq!(
            serde_json::to_value(&lane).unwrap(),
            serde_json::json!("offload.stream")
        );
        let back: NodeLane = serde_json::from_str("\"jobs.submit\"").unwrap();
        assert_eq!(back.as_str(), "jobs.submit");
        for bad in [
            "",
            "Atlas.Ingest",
            "atlas ingest",
            "atlas/ingest",
            &"a".repeat(65),
        ] {
            assert!(NodeLane::parse(bad).is_none(), "{bad:?}");
            assert!(serde_json::from_value::<NodeLane>(serde_json::json!(bad)).is_err());
        }
    }

    #[test]
    fn a_known_node_gets_only_its_own_credential() {
        let mut store = WorkstationCredentials::default();
        store.upsert(cred("ws-a", "secret-a"));
        store.upsert(cred("ws-b", "secret-b"));
        assert_eq!(store.for_node(Some("ws-b")).unwrap().credential, "secret-b");
        // A node that issued nothing is offered nothing, even with others held.
        assert_eq!(store.for_node(Some("ws-rogue")), None);
        // An unknown node id with several held is ambiguous: nothing offered.
        assert_eq!(store.for_node(None), None);
    }

    #[test]
    fn an_unknown_node_gets_the_sole_credential() {
        let mut store = WorkstationCredentials::default();
        store.upsert(cred("ws-a", "secret-a"));
        assert_eq!(store.for_node(None).unwrap().credential, "secret-a");
        assert_eq!(store.for_node(Some("")).unwrap().credential, "secret-a");
    }

    #[test]
    fn upsert_replaces_the_same_workstation() {
        let mut store = WorkstationCredentials::default();
        store.upsert(cred("ws-a", "old"));
        store.upsert(cred("ws-a", "new"));
        assert_eq!(store.workstations.len(), 1);
        assert_eq!(store.for_node(Some("ws-a")).unwrap().credential, "new");
    }

    #[test]
    fn save_is_owner_only_and_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        assert_eq!(
            WorkstationCredentials::load(&path).unwrap(),
            WorkstationCredentials::default()
        );
        let mut store = WorkstationCredentials::default();
        store.upsert(cred("ws-a", "secret-a"));
        store.save(&path).unwrap();
        assert_eq!(WorkstationCredentials::load(&path).unwrap(), store);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn a_corrupt_store_reads_as_nothing_installed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(WorkstationCredentials::load(&path).is_err());
        assert_eq!(
            WorkstationCredentials::load_or_empty(&path),
            WorkstationCredentials::default()
        );
    }
}
