//! Where the loopback backend finds its relay, read from the environment.
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `ADOS_SWARMBUS_RELAY_HOST` | `127.0.0.1` | relay host |
//! | `ADOS_SWARMBUS_RELAY_PORT` | `7900` | relay control port; data is the next port |
//! | `ADOS_SWARMBUS_NODE` | required | this node's name on the relay, unique per node |
//! | `ADOS_SWARMBUS_NODE_ADDR` | unset | address the relay sends to; unset uses the control connection's source |
//! | `ADOS_SWARMBUS_LISTEN` | `0.0.0.0:0` | local downlink socket; port 0 picks one and registers it |
//!
//! The node name must differ from any other registration on the relay, including
//! the same host's video and control lanes: the relay keeps one downlink address
//! per node name, so sharing a name would redirect those lanes' traffic here.

use std::io;
use std::net::SocketAddr;

pub const ENV_RELAY_HOST: &str = "ADOS_SWARMBUS_RELAY_HOST";
pub const ENV_RELAY_PORT: &str = "ADOS_SWARMBUS_RELAY_PORT";
pub const ENV_NODE: &str = "ADOS_SWARMBUS_NODE";
pub const ENV_NODE_ADDR: &str = "ADOS_SWARMBUS_NODE_ADDR";
pub const ENV_LISTEN: &str = "ADOS_SWARMBUS_LISTEN";

pub const DEFAULT_RELAY_HOST: &str = "127.0.0.1";
pub const DEFAULT_RELAY_PORT: u16 = 7900;
const DEFAULT_LISTEN: &str = "0.0.0.0:0";

/// The relay datagram carries the node name behind a one-byte length.
pub const MAX_NODE_LEN: usize = 255;

/// The loopback backend's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopbackConfig {
    pub relay_host: String,
    /// The control port; the data port is one above it.
    pub relay_port: u16,
    pub node: String,
    pub node_addr: Option<String>,
    pub listen: SocketAddr,
}

impl LoopbackConfig {
    /// Read the settings through `get`, which maps a variable name to its value.
    ///
    /// A missing node name or an unparseable value is an `InvalidInput` error
    /// naming the variable, never a silent default.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> io::Result<Self> {
        let value = |key: &str| {
            get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let node = value(ENV_NODE).ok_or_else(|| invalid(format!("{ENV_NODE} is not set")))?;
        if node.len() > MAX_NODE_LEN {
            return Err(invalid(format!(
                "{ENV_NODE} is {} bytes, the relay allows {MAX_NODE_LEN}",
                node.len()
            )));
        }
        let relay_port = match value(ENV_RELAY_PORT) {
            None => DEFAULT_RELAY_PORT,
            Some(p) => match p.parse::<u16>() {
                Ok(port) if port < u16::MAX => port,
                _ => {
                    return Err(invalid(format!(
                        "{ENV_RELAY_PORT}={p} is not a control port"
                    )))
                }
            },
        };
        let listen_text = value(ENV_LISTEN).unwrap_or_else(|| DEFAULT_LISTEN.to_string());
        let listen = listen_text
            .parse::<SocketAddr>()
            .map_err(|_| invalid(format!("{ENV_LISTEN}={listen_text} is not ip:port")))?;
        Ok(Self {
            relay_host: value(ENV_RELAY_HOST).unwrap_or_else(|| DEFAULT_RELAY_HOST.to_string()),
            relay_port,
            node,
            node_addr: value(ENV_NODE_ADDR),
            listen,
        })
    }
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}
