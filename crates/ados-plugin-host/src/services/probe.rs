//! Readiness probes for plugin services.

use super::*;

/// The readiness probe for a command `ready_check`: the argv run in the
/// plugin's install dir with the plugin's resource envelope and capability
/// sandbox.
pub fn probe_spec(
    manifest: &PluginManifest,
    argv: &[String],
    paths: &Paths,
    granted: &BTreeSet<String>,
    loopback_guard_active: bool,
) -> Result<ProbeSpec, SupervisorError> {
    let working_dir = paths.install_dir.join(&manifest.id);
    path_token("install dir", &working_dir)?;
    let mut directives = sandbox_directives(granted, loopback_guard_active, &[]);
    directives.extend(fc_port_exclusion(granted, fc_serial_port().as_deref()));
    directives.extend(capture_root_bind(granted, capture_root().as_deref()));
    Ok(ProbeSpec {
        argv: argv.to_vec(),
        working_dir,
        user: crate::plugin_account::plugin_user_for(&manifest.id),
        resources: manifest
            .agent
            .as_ref()
            .map(|a| a.resources.clone())
            .unwrap_or_default(),
        exec_paths: exec_paths_for(manifest, paths)?,
        sandbox_directives: directives,
    })
}

/// GET a loopback URL; ready on a 2xx status.
pub fn probe_http(port: u16, path: &str, https: bool) -> (bool, Option<String>) {
    if https {
        // The probe is made by the agent over loopback; TLS there buys nothing
        // and this host carries no TLS client.
        return (
            false,
            Some("https ready_check is not supported; use http".to_string()),
        );
    }
    let attempt = || -> std::io::Result<u16> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let mut stream = std::net::TcpStream::connect_timeout(&addr, HTTP_PROBE_TIMEOUT)?;
        stream.set_read_timeout(Some(HTTP_PROBE_TIMEOUT))?;
        stream.set_write_timeout(Some(HTTP_PROBE_TIMEOUT))?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {LOOPBACK_HOST}:{port}\r\nConnection: close\r\n\r\n"
        )?;
        let mut head = [0u8; 64];
        let n = stream.read(&mut head)?;
        let line = String::from_utf8_lossy(&head[..n]);
        line.split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| std::io::Error::other("no HTTP status line"))
    };
    match attempt() {
        Ok(code) if (200..300).contains(&code) => (true, None),
        Ok(code) => (false, Some(format!("http status {code}"))),
        Err(e) => (false, Some(format!("http probe failed: {e}"))),
    }
}
