//! The router's PIC gate, evaluated by the host before a command is queued.
//!
//! With injector arbitration armed, the host's router links declare themselves
//! an autonomous injector, and the router drops their commands whenever the PIC
//! arbiter hands authority to the human hold. The drop happens after the host
//! has queued the bytes and says nothing back on the socket, so a plugin used to
//! read `sent: true` for a command the flight controller never saw. The host now
//! asks the same question first, from the same arbiter sidecar and the same
//! decision ([`ados_hid::pic_view::injector_refused`]), and answers
//! `sent: false, reason: "pic_refused"` instead of queueing.
//!
//! The router's own gate still enforces; this only makes its verdict visible.
//! The two reads are taken independently, so a claim that changes inside the
//! router's short PIC cache window can still be judged differently for one
//! command.

use std::path::PathBuf;
use std::time::SystemTime;

use ados_hid::pic_view::{injector_refused, read_pic_view};

/// The host's view of the router's PIC gate for one declared injector identity.
pub struct PicGate {
    pic_state_path: PathBuf,
    injector_id: String,
}

impl PicGate {
    /// A gate for the injector `injector_id` the host declares, reading the PIC
    /// arbiter's sidecar at its runtime path.
    pub fn new(injector_id: impl Into<String>) -> Self {
        Self::with_path(ados_hid::paths::pic_state_json(), injector_id)
    }

    /// A gate reading the sidecar at `pic_state_path`.
    pub fn with_path(pic_state_path: PathBuf, injector_id: impl Into<String>) -> Self {
        Self {
            pic_state_path,
            injector_id: injector_id.into(),
        }
    }

    /// Whether the router would refuse this injector's command right now: a
    /// human (or any other client) holds PIC, or the arbiter is not reporting.
    ///
    /// The host's claim is taken as verified. It is minted from the same
    /// pairing key the router verifies it against, once per connection.
    pub fn refuses(&self) -> bool {
        let pic = read_pic_view(&self.pic_state_path, SystemTime::now());
        injector_refused(pic.as_ref(), Some(&self.injector_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_with(sidecar: Option<&str>) -> (tempfile::TempDir, PicGate) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pic-state.json");
        if let Some(body) = sidecar {
            std::fs::write(&path, body).unwrap();
        }
        (dir, PicGate::with_path(path, "plugin-host"))
    }

    #[test]
    fn a_human_holding_pic_refuses_the_host() {
        let (_dir, gate) = gate_with(Some(r#"{"state":"claimed","claimed_by":"hdmi-kiosk"}"#));
        assert!(gate.refuses());
    }

    #[test]
    fn an_arbiter_that_is_not_reporting_refuses_the_host() {
        let (_dir, gate) = gate_with(None);
        assert!(gate.refuses());
    }

    #[test]
    fn an_unclaimed_pic_or_the_hosts_own_claim_lets_it_through() {
        let (_dir, gate) = gate_with(Some(r#"{"state":"unclaimed"}"#));
        assert!(!gate.refuses());
        let (_dir, gate) = gate_with(Some(r#"{"state":"claimed","claimed_by":"plugin-host"}"#));
        assert!(!gate.refuses());
    }
}
