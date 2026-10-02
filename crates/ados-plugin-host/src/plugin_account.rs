//! Per-plugin system accounts.
//!
//! Every installed plugin runs under its own system user,
//! `ados-plg-<first 8 hex of sha256(plugin id)>`, whose primary group is the
//! shared [`PLUGIN_GROUP`]. A shared uid would let one plugin read another's
//! `/proc/<pid>/environ`, ptrace it, or reach its host socket through
//! `/proc/<pid>/root`, and so act with the other plugin's grants; distinct
//! uids put each plugin behind the kernel's own process isolation. The shared
//! primary group is what the loopback guard and the router match a plugin
//! process by (`meta skgid`, `SO_PEERCRED`).
//!
//! The user is created at install and removed at uninstall. Only the plugin
//! host may write the account database (its unit can write `/etc`; the other
//! lifecycle callers run under `ProtectSystem=strict`), so a controller in
//! another process asks the host over its control socket ([`HostAccounts`])
//! and the host does the work itself ([`LocalAccounts`]). Anywhere the caller
//! is not root (a dev host, the macOS workstation, tests) the account calls
//! are no-ops and units run as the invoking user.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::errors::SupervisorError;

pub use ados_protocol::ipc::PLUGIN_GROUP;
pub use ados_protocol::vision_rpc::VISION_READERS_GROUP;

/// Prefix of every per-plugin system user name.
pub const PLUGIN_USER_PREFIX: &str = "ados-plg-";

/// How long one account-management command may take.
const ACCOUNT_CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// The first 8 hex digits of `sha256(plugin_id)`: a stable, fixed-width tag that
/// tells two ids apart even when their sanitized forms collide.
pub fn id_hash8(plugin_id: &str) -> String {
    hex::encode(&Sha256::digest(plugin_id.as_bytes())[..4])
}

/// The system user `plugin_id` runs as: `ados-plg-<8 hex>`, 17 characters, well
/// inside the 32-character user-name limit.
pub fn plugin_user_for(plugin_id: &str) -> String {
    format!("{PLUGIN_USER_PREFIX}{}", id_hash8(plugin_id))
}

/// Creates and removes plugin accounts and resolves their ids.
pub trait PluginAccounts: Send + Sync {
    /// Make sure `plugin_id`'s user exists, in [`PLUGIN_GROUP`].
    fn ensure(&self, plugin_id: &str) -> Result<(), SupervisorError>;
    /// Remove `plugin_id`'s user. An absent user is not an error.
    fn remove(&self, plugin_id: &str) -> Result<(), SupervisorError>;
    /// Hand `dir` (never following a symlink) to `plugin_id`'s user and
    /// [`PLUGIN_GROUP`].
    fn hand_over(&self, dir: &Path, plugin_id: &str) -> Result<(), SupervisorError>;
}

/// The shared account manager.
pub type Accounts = Arc<dyn PluginAccounts>;

/// Creates and removes accounts in this process with `useradd`/`userdel`.
/// Used by the plugin host, whose unit may write the account database.
pub struct LocalAccounts;

/// Asks the plugin host to create and remove accounts through its control
/// socket; hands dirs over in this process. Used by every other lifecycle
/// caller.
pub struct HostAccounts {
    control_dir: PathBuf,
}

/// The account manager the plugin host uses for itself.
pub fn local_accounts() -> Accounts {
    Arc::new(LocalAccounts)
}

/// The account manager a lifecycle controller outside the plugin host uses.
pub fn host_accounts(control_dir: &Path) -> Accounts {
    Arc::new(HostAccounts {
        control_dir: control_dir.to_path_buf(),
    })
}

fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

fn run(program: &str, args: &[&str]) -> Result<(), SupervisorError> {
    crate::backend::run_bounded(program, args, None, ACCOUNT_CALL_TIMEOUT).map(|_| ())
}

/// Create the shared plugin groups when absent: [`PLUGIN_GROUP`], every
/// plugin's primary group, and [`VISION_READERS_GROUP`], which a plugin joins
/// with its frame-read grant. Root only; a no-op elsewhere.
pub fn ensure_groups() -> Result<(), SupervisorError> {
    if !is_root() {
        return Ok(());
    }
    for group in [PLUGIN_GROUP, VISION_READERS_GROUP] {
        if nix::unistd::Group::from_name(group)
            .ok()
            .flatten()
            .is_none()
        {
            run("groupadd", &["--system", group])?;
        }
    }
    Ok(())
}

/// The uid of `plugin_id`'s user, when it exists.
pub fn plugin_uid(plugin_id: &str) -> Option<u32> {
    nix::unistd::User::from_name(&plugin_user_for(plugin_id))
        .ok()
        .flatten()
        .map(|u| u.uid.as_raw())
}

impl PluginAccounts for LocalAccounts {
    fn ensure(&self, plugin_id: &str) -> Result<(), SupervisorError> {
        if !is_root() {
            return Ok(());
        }
        ensure_groups()?;
        let user = plugin_user_for(plugin_id);
        if nix::unistd::User::from_name(&user).ok().flatten().is_some() {
            return Ok(());
        }
        run(
            "useradd",
            &[
                "--system",
                "--gid",
                PLUGIN_GROUP,
                "--no-create-home",
                "--home-dir",
                "/nonexistent",
                "--shell",
                "/usr/sbin/nologin",
                "--comment",
                &format!("ADOS plugin {plugin_id}"),
                &user,
            ],
        )
    }

    fn remove(&self, plugin_id: &str) -> Result<(), SupervisorError> {
        if !is_root() {
            return Ok(());
        }
        let user = plugin_user_for(plugin_id);
        if nix::unistd::User::from_name(&user).ok().flatten().is_none() {
            return Ok(());
        }
        run("userdel", &[&user])
    }

    fn hand_over(&self, dir: &Path, plugin_id: &str) -> Result<(), SupervisorError> {
        chown_to_plugin(dir, plugin_id)
    }
}

impl PluginAccounts for HostAccounts {
    fn ensure(&self, plugin_id: &str) -> Result<(), SupervisorError> {
        if !is_root() || plugin_uid(plugin_id).is_some() {
            return Ok(());
        }
        crate::control_client::account(&self.control_dir, plugin_id, true).map_err(|e| {
            SupervisorError(format!(
                "plugin {plugin_id}: the plugin host could not create its system user: {e}"
            ))
        })
    }

    fn remove(&self, plugin_id: &str) -> Result<(), SupervisorError> {
        if !is_root() || plugin_uid(plugin_id).is_none() {
            return Ok(());
        }
        crate::control_client::account(&self.control_dir, plugin_id, false).map_err(|e| {
            SupervisorError(format!(
                "plugin {plugin_id}: the plugin host could not remove its system user: {e}"
            ))
        })
    }

    fn hand_over(&self, dir: &Path, plugin_id: &str) -> Result<(), SupervisorError> {
        chown_to_plugin(dir, plugin_id)
    }
}

/// `dir` itself (a symlink is never followed) to `plugin_id`'s user and
/// [`PLUGIN_GROUP`]. Only root can hand a dir over; any other process leaves
/// it with itself. A missing account is an error: a unit that cannot write
/// its own dir would start broken.
fn chown_to_plugin(dir: &Path, plugin_id: &str) -> Result<(), SupervisorError> {
    if !is_root() {
        return Ok(());
    }
    let user = nix::unistd::User::from_name(&plugin_user_for(plugin_id))
        .ok()
        .flatten();
    let group = nix::unistd::Group::from_name(PLUGIN_GROUP).ok().flatten();
    match (user, group) {
        (Some(u), Some(g)) => {
            std::os::unix::fs::lchown(dir, Some(u.uid.as_raw()), Some(g.gid.as_raw()))
                .map_err(|e| SupervisorError(format!("chown {}: {e}", dir.display())))
        }
        _ => Err(SupervisorError(format!(
            "plugin {plugin_id}: its system account is missing; {} stays root-owned",
            dir.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_plugin_gets_a_distinct_fixed_width_user() {
        let a = plugin_user_for("com.example.thermal");
        let b = plugin_user_for("com.example-thermal");
        assert_ne!(a, b, "ids whose sanitized forms collide still differ");
        assert!(a.starts_with(PLUGIN_USER_PREFIX));
        assert_eq!(a.len(), PLUGIN_USER_PREFIX.len() + 8);
        assert!(a[PLUGIN_USER_PREFIX.len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a, plugin_user_for("com.example.thermal"));
    }
}
