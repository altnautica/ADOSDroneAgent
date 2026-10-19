//! Live device-state reader that builds the page [`PageContext`].
//!
//! The native-resolution page UI in [`crate::pages`] reads from a single
//! [`PageContext`]; this module is the seam that fills it from the running
//! agent:
//!
//! * The agent's local REST API on `127.0.0.1:8080` — the ground-station
//!   status snapshot (`/api/v1/ground-station/status`, the union of link /
//!   network / system / role / mesh / video the dashboard reads) and the setup
//!   wizard state (`/api/v1/setup/status`: completion, next step, device
//!   identity, LAN host, the advertised Mission Control URL and the hardware
//!   checklist), plus the WFB radio-pair read (`/api/wfb/pair`) for the
//!   auto-pair flag the pair-drone page shows. Authentication is the `X-ADOS-Key` header read from
//!   `/etc/ados/pairing.json`; the pairing routes stay reachable while
//!   unpaired, and an empty key is correct for an unpaired box.
//! * The `/run/ados/hop-supervisor.json` sidecar the channel-hops surface reads
//!   directly because the data lives cross-process (the radio service owns
//!   it), used only while fresh.
//! * The logging store's query socket, for the diagnostics log tail
//!   ([`crate::log_tail`]), and the HAL board sidecar plus sysfs for the
//!   identity rows ([`crate::host_identity`]).
//!
//! The history buffers the sparkline surfaces read (RSSI, CPU, temperature,
//! battery) are not carried in any one snapshot; the source keeps a rolling
//! 60-sample ring per series and pushes the freshest reading only on the timed
//! poll, so the x-axis is a regular 60 x poll-period window no matter how often
//! a route change asks for a refresh.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::host_identity::{HostIdentity, HostPaths};
use crate::pages::{
    CloudCtx, DeviceCtx, DroneCtx, FcCtx, HardwareItem, HopEntry, HoppingCtx, LinkCtx, MeshCtx,
    MeshPeer, NetworkCtx, PageContext, PairedDroneCtx, PairingCtx, RoleCtx, SystemCtx, UplinkCtx,
    VideoCtx, WifiClientCtx, LINK_STATE_STALE,
};

/// The agent's local HTTP API base. Matches the Python LCD service's
/// `http://{api_host}:{api_port}` default (`127.0.0.1:8080`).
pub const DEFAULT_API_BASE: &str = "http://127.0.0.1:8080";

/// Where the network daemon writes the access point's passphrase, 0600.
/// Mirrors `ados_net::paths::AP_PASSPHRASE_PATH`; duplicated rather than
/// depended on, because the display crate does not otherwise link the network
/// daemon and one path constant is a cheaper coupling than a whole crate.
const AP_PASSPHRASE_PATH: &str = "/etc/ados/ap-passphrase";

/// Pairing-key file. The agent writes the persisted `X-ADOS-Key` here; the LCD
/// process reads it so its status polls authenticate against a paired agent
/// instead of getting 401'd (which would leave the panel rendering blanks).
pub const PAIRING_JSON_PATH: &str = "/etc/ados/pairing.json";

/// Per-request timeout for the local status polls. Matches the Python LCD
/// service's `httpx` 0.9 s ceiling so a wedged agent never stalls the panel.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(900);

/// Rolling trend-buffer length for the sparkline surfaces (RSSI, CPU, temp,
/// battery). The pages render the last 60 samples; the source keeps exactly
/// that many, oldest first.
const HISTORY_LEN: usize = 60;

/// How old `hop-supervisor.json` may be and still describe a running
/// supervisor. Its writer refreshes it every 5 s, so three missed writes mean
/// the supervisor is not running and its last history is not current.
const HOP_SIDECAR_FRESH: Duration = Duration::from_secs(15);

/// `/run/ados` sidecar paths the surfaces read directly.
fn hop_supervisor_path() -> PathBuf {
    PathBuf::from("/run/ados/hop-supervisor.json")
}

/// A fixed-length rolling history of optional samples, oldest first. A `None`
/// push marks a gap so the sparkline can break the line, matching how the
/// Python trend buffers stored a sentinel for a missing reading.
#[derive(Debug, Clone, Default)]
struct History {
    samples: Vec<Option<f64>>,
}

impl History {
    fn push(&mut self, value: Option<f64>) {
        self.samples.push(value);
        if self.samples.len() > HISTORY_LEN {
            let overflow = self.samples.len() - HISTORY_LEN;
            self.samples.drain(..overflow);
        }
    }

    fn to_vec(&self) -> Vec<Option<f64>> {
        self.samples.clone()
    }
}

/// Reads the live agent state and composes a [`PageContext`] each refresh.
///
/// One instance is owned by the render mode in the daemon. It holds the HTTP
/// agent (reused connection pool), the resolved api-key, the resolved hostname,
/// and the rolling trend buffers that persist across refreshes.
pub struct StateSource {
    base: String,
    api_key: Option<String>,
    agent: ureq::Agent,
    hostname: String,
    hop_path: PathBuf,
    logd_socket: PathBuf,
    host_paths: HostPaths,
    rssi_history: History,
    cpu_history: History,
    temp_history: History,
    battery_history: History,
}

impl StateSource {
    /// Build a source against the default local agent, resolving the api-key
    /// from `/etc/ados/pairing.json` and the hostname from the OS.
    pub fn new() -> Self {
        Self::with_paths(
            DEFAULT_API_BASE,
            Path::new(PAIRING_JSON_PATH),
            hop_supervisor_path(),
        )
    }

    /// Build a source with explicit base URL and sidecar paths (used by tests
    /// so the polls and reads round-trip without touching `/run` or `/etc`).
    pub fn with_paths(base: impl Into<String>, pairing_json: &Path, hop_path: PathBuf) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            api_key: load_api_key(pairing_json),
            agent,
            hostname: read_hostname(),
            hop_path,
            logd_socket: PathBuf::from(crate::log_tail::LOGD_QUERY_SOCKET),
            host_paths: HostPaths::default(),
            rssi_history: History::default(),
            cpu_history: History::default(),
            temp_history: History::default(),
            battery_history: History::default(),
        }
    }

    /// GET `path` on the agent's local API and parse the JSON body. Returns
    /// `None` on any transport / status / decode error so the panel keeps the
    /// last good frame instead of going blank while the agent restarts.
    fn get_json(&self, path: &str) -> Option<Value> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.get(&url);
        if let Some(ref key) = self.api_key {
            req = req.set("X-ADOS-Key", key);
        }
        match req.call() {
            Ok(resp) => resp.into_json::<Value>().ok(),
            Err(_) => None,
        }
    }

    /// Refresh every source and build the full [`PageContext`] for this tick.
    ///
    /// The status snapshot and the setup snapshot come from the agent's local
    /// API; the hop-supervisor sidecar, the log tail and the host identity are
    /// read on-box. Each source is independent — a missing one degrades only the
    /// surfaces that read it, never the whole frame. `advance_history` is true
    /// only for the timed poll; a refresh-now (route change, ack) re-reads the
    /// state but leaves the trend buffers on their regular time base.
    pub fn build_context(&mut self, advance_history: bool) -> PageContext {
        let status = self.get_json("/api/v1/ground-station/status");
        let setup = self.get_json("/api/v1/setup/status");
        let hop = read_run_json(&self.hop_path)
            .filter(|_| file_age(&self.hop_path).is_some_and(|age| age <= HOP_SIDECAR_FRESH));
        // Best-effort schema-drift signal (never reject): warn when the
        // hop-supervisor sidecar was written by an agent with a different schema
        // version, then render anyway. The writer const lives in the radio crate,
        // so compare against the shared registry.
        if let Some(v) = hop.as_ref() {
            let got = v.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
            if let Some(ours) = ados_protocol::contracts::sidecar_version("hop-supervisor") {
                ados_protocol::sidecar::check_sidecar_version("hop-supervisor", got, ours);
            }
        }
        let mut ctx = self.map_sources(status.as_ref(), setup.as_ref(), hop.as_ref());
        if advance_history {
            self.advance_history(&ctx);
        }
        self.attach_history(&mut ctx);
        ctx.paired_drone.auto_pair_enabled =
            auto_pair_enabled(self.get_json("/api/wfb/pair").as_ref());
        apply_host_identity(&mut ctx.device, HostIdentity::read(&self.host_paths));
        ctx.diagnostics.agent_logs = crate::log_tail::fetch(&self.logd_socket).unwrap_or_default();
        ctx.sensing = self.sensing_ctx();
        self.apply_gs_extras(&mut ctx);
        ctx
    }

    /// The narrow read the small status OLED needs: the ground-station status
    /// snapshot (the link, pairing, role and network blocks the OLED lines
    /// show) plus the GNSS, radio-adapter and LoRa routes its service line and
    /// rotating screens read. No setup or radio-pair call, no log-tail query,
    /// no host-identity reads, and no trend-buffer advance.
    pub fn status_only(&mut self) -> PageContext {
        let status = self.get_json("/api/v1/ground-station/status");
        let mut ctx = self.map_sources(status.as_ref(), None, None);
        self.apply_gs_extras(&mut ctx);
        ctx
    }

    /// Map the three already-fetched JSON sources into a [`PageContext`],
    /// advancing the rolling history buffers. Split out from
    /// [`StateSource::build_context`] so the mapping is unit-testable without a
    /// live agent.
    pub fn compose(
        &mut self,
        status: Option<&Value>,
        setup: Option<&Value>,
        hop: Option<&Value>,
    ) -> PageContext {
        let mut ctx = self.map_sources(status, setup, hop);
        self.advance_history(&ctx);
        self.attach_history(&mut ctx);
        ctx
    }

    /// Map the fetched sources into a context with empty trend series.
    fn map_sources(
        &self,
        status: Option<&Value>,
        setup: Option<&Value>,
        hop: Option<&Value>,
    ) -> PageContext {
        let mut ctx = PageContext {
            hostname: self.hostname.clone(),
            clock: current_clock(),
            ..PageContext::default()
        };

        if let Some(s) = status {
            ctx.link = link_ctx(get(s, "link"));
            ctx.drone = drone_ctx(get(s, "drone").or_else(|| get(s, "paired_drone")));
            ctx.paired_drone = paired_drone_ctx(get(s, "paired_drone"));
            ctx.fc = fc_ctx(get(s, "fc"));
            ctx.cloud = cloud_ctx(get(s, "cloud"));
            ctx.pairing = pairing_ctx(get(s, "pairing"));
            ctx.role = role_ctx(get(s, "role"));
            ctx.mesh = mesh_ctx(get(s, "mesh"));
            ctx.network = network_ctx(get(s, "network"));
            ctx.uplink = uplink_ctx(get(s, "uplink").or_else(|| get(s, "modem")));
            ctx.system = system_ctx(get(s, "system"));
            ctx.video = video_ctx(get(s, "video"));
        }

        if let Some(su) = setup {
            apply_setup(&mut ctx, su);
        }

        ctx.hopping = hopping_ctx(hop, ctx.link.channel);
        ctx.device = self.device_ctx(setup, &ctx.system);
        ctx
    }

    /// Push this poll's fresh readings onto the rolling trend buffers. Each
    /// series breaks its line on a missing reading (None push).
    fn advance_history(&mut self, ctx: &PageContext) {
        self.rssi_history.push(ctx.link.rssi_dbm);
        self.cpu_history.push(ctx.system.cpu_pct);
        self.temp_history.push(ctx.system.temp_c);
        self.battery_history.push(ctx.fc.battery_remaining);
    }

    /// Copy the current trend buffers into the context.
    fn attach_history(&self, ctx: &mut PageContext) {
        ctx.link.rssi_history = self.rssi_history.to_vec();
        ctx.system.cpu_history = self.cpu_history.to_vec();
        ctx.system.temp_history = self.temp_history.to_vec();
        ctx.fc.battery_history = self.battery_history.to_vec();
    }

    /// Compose the setup-status identity fields (id, name, version) with the
    /// on-disk device-id breadcrumb and the system block's version.
    fn device_ctx(&self, setup: Option<&Value>, system: &SystemCtx) -> DeviceCtx {
        let mut device = DeviceCtx::default();
        if let Some(su) = setup {
            device.device_id = string_field(su, "device_id");
            device.device_name = string_field(su, "device_name");
            device.version = string_field(su, "version");
        }
        if device.device_id.is_none() {
            let id = ados_protocol::identity::device_id(None);
            if !id.is_empty() {
                device.device_id = Some(id);
            }
        }
        if device.version.is_none() {
            device.version = system.agent_version.clone();
        }
        device
    }
}

/// Fill the identity rows read off the box.
fn apply_host_identity(device: &mut DeviceCtx, host: HostIdentity) {
    device.board_name = host.board_name;
    device.mac_wired = host.mac_wired;
    device.mac_wireless = host.mac_wireless;
    device.primary_ip = host.primary_ip;
    device.primary_mac = host.primary_mac;
}

impl Default for StateSource {
    fn default() -> Self {
        Self::new()
    }
}

// ── JSON helpers ────────────────────────────────────────────────────

mod mappers;
use mappers::*;
mod gs_extra;
mod sensing;

/// Read the `X-ADOS-Key` from `pairing.json`. `None` when the file is absent,
/// unreadable, malformed, or carries no `api_key` — all of which are the
/// unpaired case where the header should simply be omitted.
pub(crate) fn load_api_key(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let blob: Value = serde_json::from_str(&text).ok()?;
    blob.get("api_key")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// Read a `/run/ados` JSON sidecar into a `Value`. `None` on a missing,
/// unreadable, or non-JSON file — the surface that reads it then falls back to
/// its empty state, mirroring the Python `_read_run_json` best-effort drain.
fn read_run_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// How long ago `path` was last modified. `None` when it cannot be read or its
/// time is in the future.
fn file_age(path: &Path) -> Option<Duration> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()
}

/// Read a small one-line breadcrumb file (device-id, build stamp), trimmed.
/// Empty string on any read error.
fn read_trimmed(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Resolve the node hostname for the top status bar + setup URL. Reads
/// `/etc/hostname` first (cheap, no syscall surprises in a container), then
/// falls back to the `HOSTNAME` env, then a stable placeholder.
fn read_hostname() -> String {
    let from_file = read_trimmed(Path::new("/etc/hostname"));
    if !from_file.is_empty() {
        return from_file;
    }
    if let Ok(h) = std::env::var("HOSTNAME") {
        let h = h.trim().to_string();
        if !h.is_empty() {
            return h;
        }
    }
    "ados".to_string()
}

/// The local wall clock as `HH:MM:SS`, matching the Python top bar's
/// `time.strftime("%H:%M:%S")`. Uses the system local offset; if the offset
/// can't be determined on this thread (the `time` crate refuses it in some
/// multi-threaded contexts) it falls back to UTC rather than blanking.
fn current_clock() -> String {
    use time::OffsetDateTime;
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second())
}

#[cfg(test)]
mod tests;
