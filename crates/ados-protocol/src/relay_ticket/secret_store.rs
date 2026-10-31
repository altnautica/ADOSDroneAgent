//! Relay peer secret acceptance and on-disk installation.

use super::*;

/// Why a secret may or may not be accepted from the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptDecision {
    /// No secret held: take this one. Trust-on-first-use.
    Accept,
    /// Already holding this exact secret. A no-op, not a conflict — the ground
    /// station restates it on every reconcile tick.
    AlreadyHeld,
    /// Holding a DIFFERENT secret. Refused.
    RefusedWouldOverwrite,
    /// The offered value is not a usable secret.
    RefusedMalformed,
}

/// Decide whether to accept a relay secret offered over the relay itself.
///
/// **This is the security decision in the whole scheme, so it is a pure
/// function with its own tests rather than a branch inside an HTTP handler.**
///
/// The delivery necessarily rides the very channel the credential will later
/// protect, which today is unauthenticated. If the drone simply took whatever
/// it was handed, anyone within radio range could overwrite the secret with
/// their own and then mint tickets the drone accepts — leaving a credential
/// that looks like protection on every status surface while granting exactly
/// the access it was built to deny. That is strictly worse than having none.
///
/// So: **first write wins.** An unset drone takes the first secret it is
/// offered; a drone already holding one refuses to replace it over the relay.
/// The exposure is the pairing moment rather than every request forever, which
/// is the honest statement of what trust-on-first-use buys.
///
/// Replacing a secret deliberately — a re-pair to a different ground station —
/// goes through unpair, which clears it locally, rather than through a write
/// that a stranger could also make.
pub fn decide_accept(held: Option<&str>, offered: &str) -> AcceptDecision {
    if offered.len() != RELAY_SECRET_LEN * 2 || !offered.chars().all(|c| c.is_ascii_hexdigit()) {
        return AcceptDecision::RefusedMalformed;
    }
    match held {
        None => AcceptDecision::Accept,
        Some(existing) if existing.eq_ignore_ascii_case(offered) => AcceptDecision::AlreadyHeld,
        Some(_) => AcceptDecision::RefusedWouldOverwrite,
    }
}

/// The secret this drone currently holds, or `None` when it holds none.
///
/// A missing file and an unreadable one are both "none", and "none" denies:
/// the drone has no credential to verify against, so the only relayed call it
/// will serve is the one that delivers the credential. An unreadable secret
/// therefore closes the relay lane rather than opening it — the opposite of a
/// permissions slip silently restoring radio-range full authority.
pub fn load_secret_at(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Serialises every offer this process applies, so two concurrent offers
/// cannot both read "none held". The hard link below is the cross-process
/// guard; this one keeps the read-decide-link sequence whole in-process.
pub(super) static RELAY_SECRET_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Create the secret's directory, owner-only.
pub(super) fn prepare_secret_dir(path: &std::path::Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Write `secret` to a fresh owner-only file at `tmp` and flush it to disk.
pub(super) fn write_private_file(tmp: &std::path::Path, secret: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(tmp)?;
    f.write_all(secret.as_bytes())?;
    f.sync_all()
}

/// Flush the directory entry for `path`, so a power cut after an accept
/// cannot lose the link that made the secret visible.
pub(super) fn sync_parent(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Make `offered` the held secret unless one already exists.
///
/// The secret is written whole to a temp sibling first and then hard-linked
/// into place: the link either creates the file with its full content or
/// fails because a file is already there, so no reader ever sees a partial or
/// empty secret and two writers cannot both win. On `AlreadyExists` the held
/// value decides the answer; an existing file that holds nothing (a leftover
/// from an older non-atomic write) is replaced atomically.
pub(super) fn install_secret(
    path: &std::path::Path,
    offered: &str,
) -> std::io::Result<AcceptDecision> {
    prepare_secret_dir(path)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()));
    if let Err(e) = write_private_file(&tmp, offered) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let outcome = match std::fs::hard_link(&tmp, path) {
        Ok(()) => Ok(AcceptDecision::Accept),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match load_secret_at(path) {
            Some(held) => Ok(decide_accept(Some(&held), offered)),
            None => std::fs::rename(&tmp, path).map(|()| AcceptDecision::Accept),
        },
        Err(e) => Err(e),
    };
    // Gone already after a rename; otherwise the link (or the refusal) made it
    // redundant.
    let _ = std::fs::remove_file(&tmp);
    if matches!(outcome, Ok(AcceptDecision::Accept)) {
        sync_parent(path)?;
    }
    outcome
}

/// Apply an offered secret under the [`decide_accept`] rule, storing it only on
/// [`AcceptDecision::Accept`].
///
/// The decision and the write are joined here so no caller can reach the write
/// without going through the rule -- the whole scheme rests on first-write-wins,
/// and a handler that stored first and asked afterwards would defeat it. The
/// write itself is atomic and exclusive (see `install_secret`), and offers
/// are serialised, so concurrent offers yield exactly one `Accept`. Blocking
/// file IO: an async caller runs it in `spawn_blocking`.
pub fn apply_offered_secret(
    path: &std::path::Path,
    offered: &str,
) -> Result<AcceptDecision, std::io::Error> {
    let _guard = RELAY_SECRET_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let decision = decide_accept(load_secret_at(path).as_deref(), offered);
    if decision != AcceptDecision::Accept {
        return Ok(decision);
    }
    install_secret(path, offered)
}
