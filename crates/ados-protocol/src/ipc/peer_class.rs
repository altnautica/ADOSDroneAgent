//! Command sockets with a reader group: peers that may connect and read, but
//! carry neither the service identity nor the operator group.

use std::io;
use std::path::Path;

use super::{bind_socket_file, group_gid, set_socket_group, OperatorListener, OPERATOR_GROUP};

/// What an admitted peer on a command socket is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerClass {
    /// Root or the serving process's own uid: an agent service.
    Service,
    /// A member of [`OPERATOR_GROUP`].
    Operator,
    /// A member of the socket's reader group only. It may read what the
    /// server offers and nothing more.
    Reader,
}

/// Classify a peer from its kernel-reported credentials, or `None` when the
/// socket must refuse it. Service first, then the operator group, then the
/// reader group, so a peer in both groups is an operator. With a group absent
/// from the host (its gid `None`) nobody is admitted through it.
///
/// Pure over the credentials so the decision is testable without a second uid.
pub fn classify_peer(
    peer_uid: u32,
    peer_gids: &[u32],
    server_uid: u32,
    operator_gid: Option<u32>,
    reader_gid: Option<u32>,
) -> Option<PeerClass> {
    if peer_uid == 0 || peer_uid == server_uid {
        return Some(PeerClass::Service);
    }
    let member = |gid: Option<u32>| gid.is_some_and(|g| peer_gids.contains(&g));
    if member(operator_gid) {
        Some(PeerClass::Operator)
    } else if member(reader_gid) {
        Some(PeerClass::Reader)
    } else {
        None
    }
}

/// Bind a command socket that `reader_group` may also open: the hygiene of
/// [`super::bind_command_socket`], but the file is handed to `reader_group`
/// (so its members can `connect()`), and the listener admits service peers,
/// operators and reader-group members, telling them apart through
/// [`OperatorListener::accept_classified`]. An operator that is not also in
/// `reader_group` cannot open the file; the installer adds the human operator
/// to the reader groups it creates.
///
/// Like [`tokio::net::UnixListener::bind`], this must be called from within a
/// Tokio runtime context.
pub fn bind_reader_socket(
    path: impl AsRef<Path>,
    mode: u32,
    reader_group: &str,
) -> io::Result<OperatorListener> {
    let path = path.as_ref();
    let inner = bind_socket_file(path, mode)?;
    let reader_gid = group_gid(reader_group);
    set_socket_group(path, reader_group, reader_gid);
    Ok(OperatorListener {
        inner,
        path: path.to_path_buf(),
        server_uid: nix::unistd::geteuid().as_raw(),
        operator_gid: group_gid(OPERATOR_GROUP),
        reader_gid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: u32 = 900;
    const OPERATOR: u32 = 988;
    const SENSORS: u32 = 977;
    const PLUGINS: u32 = 966;

    fn class(uid: u32, gids: &[u32]) -> Option<PeerClass> {
        classify_peer(uid, gids, SERVER, Some(OPERATOR), Some(SENSORS))
    }

    #[test]
    fn service_peers_are_root_and_the_server_uid() {
        assert_eq!(class(0, &[0]), Some(PeerClass::Service));
        assert_eq!(class(SERVER, &[SERVER]), Some(PeerClass::Service));
    }

    #[test]
    fn the_operator_group_outranks_the_reader_group() {
        assert_eq!(class(1000, &[1000, OPERATOR]), Some(PeerClass::Operator));
        assert_eq!(
            class(1000, &[1000, SENSORS, OPERATOR]),
            Some(PeerClass::Operator)
        );
    }

    #[test]
    fn a_reader_group_member_is_only_a_reader() {
        // A plugin: primary group the plugin group, one supplementary reader.
        assert_eq!(class(61_001, &[PLUGINS, SENSORS]), Some(PeerClass::Reader));
    }

    #[test]
    fn anyone_else_is_refused() {
        assert_eq!(class(61_001, &[PLUGINS]), None);
        assert_eq!(class(1000, &[1000]), None);
        // An absent group admits nobody through it.
        assert_eq!(
            classify_peer(61_001, &[SENSORS], SERVER, Some(OPERATOR), None),
            None
        );
        assert_eq!(classify_peer(1000, &[OPERATOR], SERVER, None, None), None);
    }

    /// A socket bound for a reader group classifies this process (the server's
    /// own uid) as a service peer through the real accept path.
    #[tokio::test]
    async fn a_reader_socket_classifies_its_own_uid_as_service() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bus.sock");
        let listener = bind_reader_socket(&path, 0o660, "ados-test-no-such-group").unwrap();
        let _client = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (_stream, class) = listener.accept_classified().await.unwrap();
        assert_eq!(class, PeerClass::Service);
    }
}
