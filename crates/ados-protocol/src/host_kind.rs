//! What kind of machine the agent is installed on: a dedicated appliance
//! (an SBC, a flight node, a kiosk box) or a personal computer (a laptop or a
//! desktop somebody also uses for everything else).
//!
//! The distinction exists because a ground station on a personal computer must
//! not take the machine away from its owner. It keeps the radio, pairing, video
//! and control services, and leaves the console, the display, the Wi-Fi station,
//! sleep, updates and the routing table alone. On an appliance all of those are
//! the agent's to own, and that is the behaviour every board shipped so far has.
//!
//! The kind is persisted in [`HOST_CONF`] by the installer and read by everyone
//! else, so it survives reboots and an upgrade does not have to re-derive it.
//! Anything unreadable, missing or unrecognised reads as [`HostKind::Appliance`]:
//! a host that cannot prove it is personal gets the appliance behaviour, which
//! is the one every existing unit set was validated against.

use std::path::{Path, PathBuf};

/// Where the resolved host kind is persisted. The body is YAML, one key:
/// `host: personal`.
pub const HOST_CONF: &str = "/etc/ados/host.conf";

/// The kind of machine the agent runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostKind {
    /// A dedicated box: the agent owns the console, display, radios and routes.
    #[default]
    Appliance,
    /// A computer somebody uses for everything else: the agent owns only the
    /// radio, pairing, video and control services.
    Personal,
}

impl HostKind {
    /// The lowercase wire string used in `host.conf`, the REST status payload
    /// and the config surface.
    pub const fn as_str(self) -> &'static str {
        match self {
            HostKind::Appliance => "appliance",
            HostKind::Personal => "personal",
        }
    }

    /// Parse the wire string. Accepts the two spellings this enum writes; every
    /// other value (including the empty string) is `None` so a corrupt
    /// `host.conf` falls back to the appliance default rather than guessing.
    pub fn parse(s: &str) -> Option<HostKind> {
        match s.trim() {
            "appliance" => Some(HostKind::Appliance),
            "personal" => Some(HostKind::Personal),
            _ => None,
        }
    }

    /// Whether this host is somebody's own computer.
    pub const fn is_personal(self) -> bool {
        matches!(self, HostKind::Personal)
    }
}

impl std::fmt::Display for HostKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `host:` value in a `host.conf` body, or `None` when the key is absent,
/// empty or unrecognised. Tolerant of `host:`/`host=`, of comments and of the
/// YAML document marker, so a hand-edited file still resolves.
pub fn parse_conf(body: &str) -> Option<HostKind> {
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line
            .strip_prefix("host:")
            .or_else(|| line.strip_prefix("host="))
        else {
            continue;
        };
        let value = rest.trim().trim_matches(['"', '\'']);
        if !value.is_empty() {
            return HostKind::parse(value);
        }
    }
    None
}

/// The host kind recorded at `path`, defaulting to [`HostKind::Appliance`].
pub fn read_host_kind_from(path: &Path) -> HostKind {
    match std::fs::read_to_string(path) {
        Ok(body) => parse_conf(&body).unwrap_or_default(),
        Err(_) => HostKind::Appliance,
    }
}

/// The host kind recorded in [`HOST_CONF`], defaulting to
/// [`HostKind::Appliance`].
pub fn read_host_kind() -> HostKind {
    read_host_kind_from(Path::new(HOST_CONF))
}

/// The body an install writes to [`HOST_CONF`].
pub fn conf_body(kind: HostKind) -> String {
    format!("host: {}\n", kind.as_str())
}

/// `HOST_CONF` as a path, for callers that log or compare it.
pub fn host_conf_path() -> PathBuf {
    PathBuf::from(HOST_CONF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_strings_round_trip() {
        for kind in [HostKind::Appliance, HostKind::Personal] {
            assert_eq!(HostKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(HostKind::parse("Personal"), None);
        assert_eq!(HostKind::parse(""), None);
        assert_eq!(HostKind::parse("workstation"), None);
    }

    #[test]
    fn missing_or_unknown_reads_as_appliance() {
        assert_eq!(
            read_host_kind_from(Path::new("/nonexistent/host.conf")),
            HostKind::Appliance
        );
        assert_eq!(parse_conf(""), None);
        assert_eq!(parse_conf("profile: ground_station\n"), None);
        assert_eq!(parse_conf("host: tower\n"), None);
    }

    #[test]
    fn the_written_body_reads_back() {
        for kind in [HostKind::Appliance, HostKind::Personal] {
            assert_eq!(parse_conf(&conf_body(kind)), Some(kind));
        }
        // A hand edit with a comment and YAML quotes still resolves.
        assert_eq!(
            parse_conf("---\n# set by the install\nhost: 'personal'\n"),
            Some(HostKind::Personal)
        );
        assert_eq!(parse_conf("host=personal\n"), Some(HostKind::Personal));
    }
}
