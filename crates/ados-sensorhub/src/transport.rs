//! Reaching the hub: a USB CDC-ACM serial node or a TCP socket (a simulated
//! hub). Both carry the same byte stream.

use std::path::Path;
use std::time::Duration;

use ados_hal_probe::bindings::{parse_vid_pid, SENSOR_HUB_VID_PID};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::config::TransportSpec;

/// A resolved hub endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Serial(String),
    Tcp(String),
}

impl Endpoint {
    /// The sidecar's `transport` value.
    pub fn label(&self) -> String {
        match self {
            Endpoint::Serial(dev) => format!("serial:{dev}"),
            Endpoint::Tcp(addr) => format!("tcp:{addr}"),
        }
    }
}

/// Resolve a transport spec. `auto` walks the `/sys/class/tty`-shaped
/// `tty_root` for the first `ttyACM*` backed by the hub firmware's USB id;
/// `None` when there is none.
pub fn resolve(spec: &TransportSpec, tty_root: &Path) -> Option<Endpoint> {
    match spec {
        TransportSpec::Serial(dev) => Some(Endpoint::Serial(dev.clone())),
        TransportSpec::Tcp(addr) => Some(Endpoint::Tcp(addr.clone())),
        TransportSpec::Auto => {
            let (vid, pid) = parse_vid_pid(SENSOR_HUB_VID_PID)?;
            ados_hal_probe::sensor_hub::find_acm_by_usb_id(tty_root, vid, pid).map(Endpoint::Serial)
        }
    }
}

/// A connected hub byte stream.
pub trait HubIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> HubIo for T {}

/// Line rate requested on the serial node. A CDC-ACM device ignores it; the
/// port is opened raw 8N1 with no flow control.
const SERIAL_BAUD: u32 = 115_200;
/// A simulated hub that does not answer the connect within this is retried.
pub const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Open the endpoint.
pub async fn open(ep: &Endpoint) -> std::io::Result<Box<dyn HubIo>> {
    match ep {
        Endpoint::Serial(dev) => {
            let port = tokio_serial::new(dev, SERIAL_BAUD)
                .data_bits(tokio_serial::DataBits::Eight)
                .parity(tokio_serial::Parity::None)
                .stop_bits(tokio_serial::StopBits::One)
                .flow_control(tokio_serial::FlowControl::None);
            let stream = tokio_serial::SerialStream::open(&port).map_err(std::io::Error::other)?;
            Ok(Box::new(stream))
        }
        Endpoint::Tcp(addr) => {
            let connect = tokio::net::TcpStream::connect(addr);
            let stream = tokio::time::timeout(TCP_CONNECT_TIMEOUT, connect)
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out")
                })??;
            stream.set_nodelay(true)?;
            Ok(Box::new(stream))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_tty(root: &Path, node: &str, vid: &str, pid: &str) {
        let dev = root.join(node);
        std::fs::create_dir_all(dev.join("device")).unwrap();
        std::fs::write(dev.join("idVendor"), vid).unwrap();
        std::fs::write(dev.join("idProduct"), pid).unwrap();
    }

    #[test]
    fn auto_picks_the_hub_firmware_node_from_sysfs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert_eq!(resolve(&TransportSpec::Auto, root), None);
        fake_tty(root, "ttyACM0", "1209", "5741");
        assert_eq!(resolve(&TransportSpec::Auto, root), None);
        fake_tty(root, "ttyACM3", "1209", "ad05");
        let ep = resolve(&TransportSpec::Auto, root).unwrap();
        assert_eq!(ep, Endpoint::Serial("/dev/ttyACM3".into()));
        assert_eq!(ep.label(), "serial:/dev/ttyACM3");
    }

    #[test]
    fn explicit_transports_resolve_without_sysfs() {
        let none = Path::new("/nonexistent");
        assert_eq!(
            resolve(&TransportSpec::Serial("/dev/ttyACM9".into()), none),
            Some(Endpoint::Serial("/dev/ttyACM9".into()))
        );
        let tcp = resolve(&TransportSpec::Tcp("127.0.0.1:7600".into()), none).unwrap();
        assert_eq!(tcp.label(), "tcp:127.0.0.1:7600");
    }
}
