//! The hop supervisor's edges: the control-plane frames it sends and receives,
//! and the radio it retunes and respawns. [`LiveRadio`] is the drone's own
//! (loopback UDP to the wfb control planes, `iw`, the radio process group, the
//! run-dir files); the deterministic simulation in `tests/dst` supplies its own,
//! so the supervisor's real decision code runs there unchanged.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::WfbConfig;
use crate::daemon::bringup::phy::{channel_from_iface, set_channel};
use crate::hop::{HOP_ACK_PORT, HOP_CONTROL_PORT};
use crate::link_quality::LinkStats;
use crate::paths::{read_bind_sentinel_active, run_path, write_sidecar, WFB_TX_KEY};
use crate::process::RadioProcesses;

/// The control plane: frames out to the transmit control plane, frames back on
/// the return port.
#[async_trait]
pub trait ControlPlane: Send + Sync + 'static {
    /// A bound return port. Dropping it releases the port.
    type Port: Send + Sync + 'static;

    /// The radio interface, for log fields.
    fn iface(&self) -> &str;

    /// Bind the control-plane return port (`127.0.0.1:5810`). Exclusive: a
    /// second bind fails while a bound port is held.
    async fn bind_return_port(&self) -> std::io::Result<Self::Port>;

    /// Wait for the next frame on `port` and copy it into `buf`.
    async fn recv_return(&self, port: &Self::Port, buf: &mut [u8]) -> std::io::Result<usize>;

    /// Send one control frame to the transmit control plane
    /// (`127.0.0.1:5803`). Best effort: a failed send is the same as a frame
    /// lost on the air.
    async fn send_control(&self, pkt: &[u8]);

    /// The channel the radio is tuned to now, `None` when it cannot be read.
    async fn live_channel(&self) -> Option<u8>;
}

/// The radio the hop supervisor moves: retune, respawn, pick a target, and the
/// facts it gates on.
#[async_trait]
pub trait HopRadio: ControlPlane {
    /// Retune the radio to `channel`, verified: `false` when it did not land.
    async fn set_channel(&self, channel: u8) -> bool;

    /// Kill and respawn the whole radio process group on the channel the radio
    /// is tuned to. `false` when the group did not come back.
    async fn respawn_group(&self) -> bool;

    /// The hop target from `current` (a live scan of the configured band,
    /// filtered by `enabled`); `current` when there is none.
    async fn pick_hop_target(&self, current: u8, enabled: Option<&BTreeSet<u8>>) -> u8;

    /// Put the interface back in monitor mode on `current` if a scan left it
    /// managed.
    async fn restore_monitor_if_needed(&self, current: u8);

    /// Whether a bind session owns the adapter now.
    fn bind_active(&self) -> bool;

    /// Whether this drone holds a transmit key (it is paired).
    fn paired(&self) -> bool;

    /// Write one run-dir sidecar.
    fn write_sidecar(&self, name: &str, value: &serde_json::Value);
}

/// The drone's control plane: loopback UDP to the local wfb control planes.
pub(crate) struct LoopbackControl {
    iface: String,
    /// The send socket; `None` for a receive-only listener.
    send: Option<tokio::net::UdpSocket>,
}

impl LoopbackControl {
    /// A control plane that sends from a fresh ephemeral socket.
    pub(crate) async fn bind(iface: &str) -> std::io::Result<Self> {
        Ok(Self {
            iface: iface.to_string(),
            send: Some(tokio::net::UdpSocket::bind("0.0.0.0:0").await?),
        })
    }

    /// A control plane that only listens on the return port.
    pub(crate) fn receive_only() -> Self {
        Self {
            iface: String::new(),
            send: None,
        }
    }
}

#[async_trait]
impl ControlPlane for LoopbackControl {
    type Port = tokio::net::UdpSocket;

    fn iface(&self) -> &str {
        &self.iface
    }

    /// Loopback only: the one producer is the rx-control `wfb_rx` re-emitting
    /// on 127.0.0.1, and nothing on the network should be able to post into
    /// this listener.
    async fn bind_return_port(&self) -> std::io::Result<Self::Port> {
        tokio::net::UdpSocket::bind(("127.0.0.1", HOP_ACK_PORT)).await
    }

    async fn recv_return(&self, port: &Self::Port, buf: &mut [u8]) -> std::io::Result<usize> {
        port.recv_from(buf).await.map(|(n, _)| n)
    }

    async fn send_control(&self, pkt: &[u8]) {
        if let Some(sock) = &self.send {
            let _ = sock
                .send_to(pkt, format!("127.0.0.1:{HOP_CONTROL_PORT}"))
                .await;
        }
    }

    async fn live_channel(&self) -> Option<u8> {
        channel_from_iface(&self.iface).await
    }
}

/// The drone's radio: `iw` on the interface, the radio process group, the
/// bind sentinel, the transmit key and the run-dir sidecars.
pub(crate) struct LiveRadio {
    pub(crate) control: LoopbackControl,
    pub(crate) cfg: WfbConfig,
    pub(crate) proc: Arc<tokio::sync::Mutex<RadioProcesses>>,
    pub(crate) link: Arc<tokio::sync::Mutex<LinkStats>>,
}

#[async_trait]
impl ControlPlane for LiveRadio {
    type Port = tokio::net::UdpSocket;

    fn iface(&self) -> &str {
        self.control.iface()
    }

    async fn bind_return_port(&self) -> std::io::Result<Self::Port> {
        self.control.bind_return_port().await
    }

    async fn recv_return(&self, port: &Self::Port, buf: &mut [u8]) -> std::io::Result<usize> {
        self.control.recv_return(port, buf).await
    }

    async fn send_control(&self, pkt: &[u8]) {
        self.control.send_control(pkt).await
    }

    async fn live_channel(&self) -> Option<u8> {
        self.control.live_channel().await
    }
}

#[async_trait]
impl HopRadio for LiveRadio {
    async fn set_channel(&self, channel: u8) -> bool {
        set_channel(self.iface(), channel).await
    }

    /// The group respawn reuses the live data-plane tunables (manual tier /
    /// adaptive FEC), so a hop or a return home never reverts the operator's
    /// pinned link rate back to the boot-time config.
    async fn respawn_group(&self) -> bool {
        let mut p = self.proc.lock().await;
        p.respawn_group(&self.cfg, self.link.clone()).await
    }

    async fn pick_hop_target(&self, current: u8, enabled: Option<&BTreeSet<u8>>) -> u8 {
        crate::channel::pick_hop_target(self.iface(), current, &self.cfg.band, enabled).await
    }

    async fn restore_monitor_if_needed(&self, current: u8) {
        crate::adapter::restore_monitor_if_needed(self.iface(), current).await
    }

    fn bind_active(&self) -> bool {
        read_bind_sentinel_active()
    }

    fn paired(&self) -> bool {
        std::path::Path::new(WFB_TX_KEY).exists()
    }

    fn write_sidecar(&self, name: &str, value: &serde_json::Value) {
        let _ = write_sidecar(&run_path(name), value);
    }
}
