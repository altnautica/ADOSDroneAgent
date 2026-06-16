//! Runtime + on-disk path constants for the HID services.
//!
//! The PIC control socket already lives in [`crate::pic_ipc::PIC_SOCK`]; this
//! module collects the sibling runtime artifacts the input/PIC daemons own:
//!
//! * [`PIC_STATE_JSON`] — the PIC arbiter state sidecar the `ados-pic` daemon
//!   writes on every transition, so a reader (the display layer, a status route,
//!   the residual `/pic/events` consumer) can read the current holder without a
//!   socket round-trip and survive the daemon being momentarily unreachable.
//! * [`HID_CMD_SOCK`] — the `ados-input` daemon's operator command socket, the
//!   write seam for the primary-gamepad selection. The daemon owns the running
//!   hotplug tracker (the live primary), so a write that must take effect on the
//!   running state forwards here rather than only touching the on-disk sidecar.
//!
//! Paths honour the `ADOS_RUN_DIR` override (a tmpfs runtime dir, wiped on
//! reboot) so a test can redirect them; the on-disk input selection sidecar stays
//! in [`crate::sidecar::GS_INPUT_JSON`] under `/etc/ados`.

use std::path::PathBuf;

/// Runtime directory default (`ADOS_RUN_DIR`). tmpfs; wiped on reboot. The PIC
/// state sidecar + the HID command socket live under it.
pub const ADOS_RUN_DIR: &str = "/run/ados";

/// The PIC arbiter state sidecar basename. The `ados-pic` daemon writes the
/// current arbiter snapshot here on every transition + watchdog tick so a reader
/// has the live holder/counter without a socket round-trip.
pub const PIC_STATE_JSON_NAME: &str = "pic-state.json";

/// The `ados-input` command socket basename. The primary-gamepad write seam.
pub const HID_CMD_SOCK_NAME: &str = "hid-cmd.sock";

/// The default PIC state sidecar path (`/run/ados/pic-state.json`).
pub const PIC_STATE_JSON: &str = "/run/ados/pic-state.json";

/// The default HID command socket path (`/run/ados/hid-cmd.sock`).
pub const HID_CMD_SOCK: &str = "/run/ados/hid-cmd.sock";

/// The runtime dir, honouring the `ADOS_RUN_DIR` override (default
/// [`ADOS_RUN_DIR`]). The daemons + a test resolve the runtime artifacts under
/// it.
pub fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| ADOS_RUN_DIR.to_string()))
}

/// The PIC state sidecar path under the resolved runtime dir.
pub fn pic_state_json() -> PathBuf {
    run_dir().join(PIC_STATE_JSON_NAME)
}

/// The HID command socket path under the resolved runtime dir.
pub fn hid_cmd_sock() -> PathBuf {
    run_dir().join(HID_CMD_SOCK_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_literal_run_dir() {
        // The literal constants are what callers without an override (and the
        // ados-control native routes) resolve against.
        assert_eq!(PIC_STATE_JSON, "/run/ados/pic-state.json");
        assert_eq!(HID_CMD_SOCK, "/run/ados/hid-cmd.sock");
    }

    #[test]
    fn run_dir_honours_the_override() {
        // ADOS_RUN_DIR is process-global; this test mutates + restores it. The
        // crate's other path tests do not read it, so no lock is needed here.
        let prev = std::env::var("ADOS_RUN_DIR").ok();
        std::env::set_var("ADOS_RUN_DIR", "/tmp/ados-test-run");
        assert_eq!(
            pic_state_json(),
            PathBuf::from("/tmp/ados-test-run/pic-state.json")
        );
        assert_eq!(
            hid_cmd_sock(),
            PathBuf::from("/tmp/ados-test-run/hid-cmd.sock")
        );
        match prev {
            Some(v) => std::env::set_var("ADOS_RUN_DIR", v),
            None => std::env::remove_var("ADOS_RUN_DIR"),
        }
    }
}
