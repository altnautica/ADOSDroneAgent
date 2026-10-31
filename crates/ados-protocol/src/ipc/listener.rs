//! Operator-gated listener, peer credential checks and socket binding.

use super::*;

impl OperatorListener {
    /// Accept the next admitted peer. Refused peers are logged and dropped; the
    /// call keeps waiting for one that is admitted. An `accept()` error is
    /// returned to the caller's own retry policy unchanged.
    pub async fn accept(&self) -> io::Result<(UnixStream, tokio::net::unix::SocketAddr)> {
        loop {
            let (stream, addr) = self.inner.accept().await?;
            if self.classify(&stream).is_some() {
                return Ok((stream, addr));
            }
        }
    }

    /// [`accept`](Self::accept), also returning what the admitted peer is
    /// ([`classify_peer`]), so a server can narrow what each class may do.
    pub async fn accept_classified(&self) -> io::Result<(UnixStream, PeerClass)> {
        loop {
            let (stream, _) = self.inner.accept().await?;
            if let Some(class) = self.classify(&stream) {
                return Ok((stream, class));
            }
        }
    }

    pub(super) fn classify(&self, stream: &UnixStream) -> Option<PeerClass> {
        match peer_credentials(stream) {
            Ok((uid, gids)) => {
                let class = classify_peer(
                    uid,
                    &gids,
                    self.server_uid,
                    self.operator_gid,
                    self.reader_gid,
                );
                if class.is_none() {
                    tracing::warn!(
                        path = %self.path.display(),
                        peer_uid = uid,
                        "command socket refused a peer outside the operator group"
                    );
                }
                class
            }
            // The narrowest class the socket admits.
            Err(err) if peer_hung_up_before_accept(&err) => Some(if self.reader_gid.is_some() {
                PeerClass::Reader
            } else {
                PeerClass::Operator
            }),
            Err(err) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "command socket refused a peer with unreadable credentials"
                );
                None
            }
        }
    }
}

/// The connecting peer's uid and group set, as the kernel reports them. An
/// error means the platform would not answer, which the caller treats as a
/// refusal (see `peer_hung_up_before_accept` for the one exception).
pub(super) fn peer_credentials(stream: &UnixStream) -> io::Result<(u32, Vec<u32>)> {
    let cred = stream.peer_cred()?;
    #[allow(unused_mut)]
    let mut gids = vec![cred.gid()];
    #[cfg(target_os = "linux")]
    gids.extend(peer_supplementary_groups(stream));
    Ok((cred.uid(), gids))
}

/// Whether the process on the other end of `stream` is on the operator plane:
/// root, this process's own uid, or a member of [`OPERATOR_GROUP`] (the same
/// policy [`OperatorListener`] enforces). For a socket that must also accept
/// plugin-plane peers (the log ingest sink), this tells the two apart so what a
/// plugin writes is recorded as the plugin's. Unreadable credentials read as
/// not-operator, which can only narrow trust, except for the development-host
/// case `peer_hung_up_before_accept` documents.
pub fn is_operator_peer(stream: &UnixStream) -> bool {
    match peer_credentials(stream) {
        Ok((uid, gids)) => operator_peer_allowed(
            uid,
            &gids,
            nix::unistd::geteuid().as_raw(),
            group_gid(OPERATOR_GROUP),
        ),
        Err(err) => peer_hung_up_before_accept(&err),
    }
}

/// Linux records `SO_PEERCRED` at `connect()` and answers it even after the
/// peer has closed. macOS answers `LOCAL_PEERCRED` with `ENOTCONN` once the peer
/// has hung up, even while its bytes are still queued, so a client that sends
/// and closes before the accept would lose everything it sent. macOS is a
/// development host only, and the socket file mode still gates `connect()`
/// there, so a peer that already hung up is admitted on non-Linux hosts.
#[cfg(not(target_os = "linux"))]
pub(super) fn peer_hung_up_before_accept(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotConnected
}

#[cfg(target_os = "linux")]
pub(super) fn peer_hung_up_before_accept(_err: &io::Error) -> bool {
    false
}

/// The peer's supplementary groups at `connect()` time (`SO_PEERGROUPS`). An
/// error reads as no supplementary groups, which can only narrow admission.
#[cfg(target_os = "linux")]
pub(super) fn peer_supplementary_groups(stream: &UnixStream) -> Vec<u32> {
    use nix::libc;
    use std::os::fd::AsRawFd;

    const GID_SIZE: usize = std::mem::size_of::<libc::gid_t>();
    let fd = stream.as_raw_fd();
    let mut groups: Vec<libc::gid_t> = vec![0; 32];
    // One retry: on ERANGE the kernel writes the size it needs into `len`.
    for _ in 0..2 {
        let mut len = (groups.len() * GID_SIZE) as libc::socklen_t;
        // SAFETY: `groups` is a live, writable buffer of exactly `len` bytes,
        // and the kernel writes at most `len` bytes into it and updates `len`.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERGROUPS,
                groups.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc == 0 {
            groups.truncate(len as usize / GID_SIZE);
            return groups;
        }
        let needed = len as usize / GID_SIZE;
        if io::Error::last_os_error().raw_os_error() != Some(libc::ERANGE) || needed <= groups.len()
        {
            break;
        }
        groups.resize(needed, 0);
    }
    Vec::new()
}

/// Bind a command-plane socket: create the parent directory, remove a stale
/// socket left by a prior run (so `bind` cannot fail with `EADDRINUSE`), bind,
/// set the file mode, and hand the file to [`OPERATOR_GROUP`]. Every accept on
/// the returned listener re-checks the peer (see [`OperatorListener`]).
///
/// [`IpcBroadcast::bind`] and the one-shot command sockets share it so the
/// hygiene and the peer policy live in one place.
///
/// Like [`UnixListener::bind`], this must be called from within a Tokio runtime
/// context.
pub fn bind_command_socket(path: impl AsRef<Path>, mode: u32) -> io::Result<OperatorListener> {
    let path = path.as_ref();
    let inner = bind_socket_file(path, mode)?;
    let operator_gid = group_gid(OPERATOR_GROUP);
    set_socket_group(path, OPERATOR_GROUP, operator_gid);
    Ok(OperatorListener {
        inner,
        path: path.to_path_buf(),
        server_uid: nix::unistd::geteuid().as_raw(),
        operator_gid,
        reader_gid: None,
    })
}

/// Bind a socket on the plugin-reachable plane: the same hygiene as
/// [`bind_command_socket`], owned by [`PLUGIN_GROUP`] so a plugin's unit can
/// connect. There is no peer check here, so it is only for surfaces a plugin
/// may legitimately reach: the per-plugin sockets, where the server gates every
/// request on the plugin's own capability token, and the log ingest sink, which
/// only accepts log frames. Never use it for an agent command socket.
pub fn bind_plugin_socket(path: impl AsRef<Path>, mode: u32) -> io::Result<UnixListener> {
    let path = path.as_ref();
    let listener = bind_socket_file(path, mode)?;
    set_socket_group(path, PLUGIN_GROUP, group_gid(PLUGIN_GROUP));
    Ok(listener)
}

pub(super) fn bind_socket_file(path: &Path, mode: u32) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // Remove a stale socket so bind does not fail with EADDRINUSE.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(listener)
}

pub(super) fn group_gid(name: &str) -> Option<u32> {
    match nix::unistd::Group::from_name(name) {
        Ok(group) => group.map(|g| g.gid.as_raw()),
        Err(err) => {
            tracing::debug!(error = %err, group = name, "resolving socket group failed");
            None
        }
    }
}

/// Give a freshly-bound socket to `group`.
///
/// This belongs next to the mode, not in each caller, because the two are one
/// decision: every service here runs as root, so a `0o660` socket grants
/// *nobody but root* until the group owns the file.
///
/// Best-effort by design: the installer creates the groups, and on a dev host
/// where one is absent this is a quiet no-op, which leaves the socket reachable
/// by its owner only. Linux-only; a stub elsewhere.
#[cfg(target_os = "linux")]
pub(super) fn set_socket_group(path: &Path, group: &str, gid: Option<u32>) {
    let Some(gid) = gid else {
        tracing::debug!(
            group,
            "socket group not present; leaving socket group as-is"
        );
        return;
    };
    if let Err(err) = nix::unistd::chown(path, None, Some(nix::unistd::Gid::from_raw(gid))) {
        tracing::debug!(error = %err, path = %path.display(), group, "chgrp socket failed");
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn set_socket_group(_path: &Path, _group: &str, _gid: Option<u32>) {}
