//! The operator-facing HTTP socket of an `agent.http: true` plugin.
//!
//! The host creates a per-plugin directory under the agent run dir, binds it
//! into the plugin's unit, and names the socket path in
//! [`HTTP_SOCKET_ENV`]. The control plane forwards
//! `/api/plugins/{id}/x/{*rest}` to that socket as `/<rest>` (query kept,
//! WebSocket upgrades included), so every request arriving on it has already
//! been authenticated as the operator. The plugin serves plain HTTP/1.1 on the
//! listener with whatever server stack it links.

use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::UnixListener;

/// The env var the host sets on an `agent.http` plugin's unit, carrying the
/// socket path the plugin must serve HTTP on.
pub const HTTP_SOCKET_ENV: &str = "ADOS_PLUGIN_HTTP_SOCKET";

/// Mode of the bound socket: the plugin's own user and the agent group may
/// connect, which is how the control plane reaches it.
const SOCKET_MODE: u32 = 0o660;

/// The HTTP socket path from the process environment, or `None` when the host
/// did not set one (the manifest does not declare `agent.http: true`).
pub fn socket_path() -> Option<PathBuf> {
    socket_path_with(|k| std::env::var(k).ok())
}

/// [`socket_path`] over an injected env lookup. An empty or whitespace value is
/// treated as unset.
pub fn socket_path_with<F>(env: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    env(HTTP_SOCKET_ENV)
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
}

/// Bind the HTTP listener at `path`.
///
/// A socket left behind by a previous run of the plugin is removed first; any
/// other file at `path` is refused with [`io::ErrorKind::AlreadyExists`] rather
/// than deleted. The bound socket is set to mode `0660`. Must be called from
/// inside a tokio runtime.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    #[test]
    fn socket_path_reads_the_env_and_treats_blank_as_unset() {
        let set = |k: &str| (k == HTTP_SOCKET_ENV).then(|| "/run/x/http.sock".to_string());
        assert_eq!(
            socket_path_with(set),
            Some(PathBuf::from("/run/x/http.sock"))
        );
        assert_eq!(socket_path_with(|_| Some("  ".to_string())), None);
        assert_eq!(socket_path_with(|_| None), None);
    }

    #[tokio::test]
    async fn bind_replaces_a_stale_socket_and_serves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("http.sock");
        // A previous run's socket, still on disk after the process died.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());

        let listener = bind(&path).expect("rebinds over the stale socket");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_MODE);

        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4];
            conn.read_exact(&mut buf).await.unwrap();
            conn.write_all(&buf).await.unwrap();
        });
        let mut client = UnixStream::connect(&path).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut echo = [0u8; 4];
        client.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"ping");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn bind_refuses_to_delete_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("http.sock");
        std::fs::write(&path, b"not a socket").unwrap();
        let err = bind(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"not a socket");
    }

    /// The SDK and the host name the same env var; a rename on either side
    /// would leave every HTTP plugin without its socket.
    #[test]
    fn env_name_matches_the_host() {
        assert_eq!(
            HTTP_SOCKET_ENV,
            ados_plugin_host::systemd::ENV_PLUGIN_HTTP_SOCKET
        );
    }
}
