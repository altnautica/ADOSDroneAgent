//! Pairing beacon and command-poll loops spawned by the cloud relay daemon.

use super::*;

/// The local API base the beacon posts the paired transition to over loopback.
/// The API process owns `pairing.json` writes (its `PairingManager.claim` uses
/// the same `pending_api_key` the beacon registered), so the relay never writes
/// the pairing file itself.
pub(super) const LOCAL_API_BASE: &str = dispatch::loopback::LOCAL_API_BASE;

/// Spawn the pairing beacon: while UNPAIRED and only when the beacon is enabled
/// and a cloud URL is set, POST the pairing code to `{convex}/pairing/register`
/// every `beacon_interval` seconds. On a claim, persist the paired transition by
/// asking the local API process to claim (loopback `POST /api/pairing/claim`),
/// which writes `pairing.json` with the same `pending_api_key` the beacon
/// registered — so the cloud-frozen key matches the persisted key and no
/// heartbeat 401s after the claim. When the file holds no live code with its
/// pending key (an install that seeded only a code, a native unpair, or a code
/// past its lifetime), the beacon asks the local API to mint or roll it
/// (loopback `GET /api/pairing/code`, the single native writer) and re-reads.
/// Best-effort throughout: an empty cloud response or a loopback failure simply
/// means the next tick retries while still unpaired.
pub(super) fn spawn_beacon(
    config: Arc<CloudConfig>,
    http: Arc<reqwest::Client>,
    convex_url: String,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let interval = Duration::from_secs(config.pairing.beacon_interval.max(1) as u64);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    // Gate: the beacon runs only when enabled, a cloud URL is set,
                    // and the agent is still unpaired. Each re-checked per tick so
                    // a pair transition (or an operator toggle) stops it.
                    if !beacon::beacon_enabled(config.pairing.beacon_enabled)
                        || convex_url.is_empty()
                    {
                        continue;
                    }
                    let mut pairing = PairingState::load();
                    if pairing.is_paired() {
                        continue;
                    }
                    // The code + the stable pending key must both be present and
                    // the code live (the Convex handler 400s on an empty code, an
                    // empty key would break the later claim, and an expired code
                    // cannot be claimed). Otherwise have the local API mint them.
                    let now_secs = now_epoch_ms() as f64 / 1000.0;
                    if pairing.beacon_credentials(now_secs).is_none() {
                        request_local_pairing_code(&http).await;
                        pairing = PairingState::load();
                    }
                    let Some((code, api_key)) = pairing.beacon_credentials(now_secs) else {
                        continue;
                    };
                    beacon_register_once(
                        &http,
                        &convex_url,
                        &config,
                        code,
                        api_key,
                        pairing.code_expires_at_ms(),
                    )
                    .await;
                }
            }
        }
    })
}

/// Ask the local API to mint or roll the pairing code. The route persists the
/// code together with its pending API key; the caller re-reads `pairing.json`.
pub(super) async fn request_local_pairing_code(http: &reqwest::Client) {
    let url = format!("{LOCAL_API_BASE}/api/pairing/code");
    match http.get(&url).send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => tracing::debug!(
            status = r.status().as_u16(),
            "local pairing code request non-2xx"
        ),
        Err(e) => tracing::debug!(error = %e, "local pairing code request failed"),
    }
}

/// One beacon registration pass: build + POST the `/pairing/register` body, and
/// on a claimed response, drive the local claim over loopback. Best-effort.
pub(super) async fn beacon_register_once(
    http: &reqwest::Client,
    convex_url: &str,
    config: &CloudConfig,
    code: &str,
    api_key: &str,
    code_expires_at: Option<i64>,
) {
    let (board_name, board_tier, _soc, _arch, _npu) = board_base();
    let inputs = beacon::BeaconInputs {
        device_id: config.agent.device_id.clone(),
        pairing_code: code.to_string(),
        api_key: api_key.to_string(),
        name: config.agent.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        board_name,
        board_tier,
        local_ip: String::new(),
        code_expires_at,
    };
    let body = beacon::build_beacon_body(&inputs);
    let url = format!("{}/pairing/register", convex_url.trim_end_matches('/'));
    let resp = match http.post(&url).json(&body).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::debug!(status = r.status().as_u16(), "pairing beacon rejected");
            return;
        }
        Err(e) => {
            tracing::debug!(error = %e, "pairing beacon failed");
            return;
        }
    };
    let reply: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return,
    };
    if !beacon::response_claimed(&reply) {
        return;
    }
    // The cloud auto-matched / already-claimed this code. Persist the paired
    // transition through the API process so it owns the pairing.json write (it
    // claims with the same pending_api_key we just registered).
    let owner = reply
        .get("userId")
        .and_then(|v| v.as_str())
        .or_else(|| reply.get("ownerId").and_then(|v| v.as_str()))
        .unwrap_or("cloud")
        .to_string();
    let claim_url = format!("{LOCAL_API_BASE}/api/pairing/claim");
    match http
        .post(&claim_url)
        .json(&serde_json::json!({ "user_id": owner }))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            tracing::info!(owner = %owner, "beacon_claimed");
        }
        Ok(r) => {
            // A non-2xx (e.g. a 409 already-paired race) is benign: the next tick
            // re-reads the pair state and stops beaconing once it is paired.
            tracing::debug!(status = r.status().as_u16(), "beacon local claim non-2xx");
        }
        Err(e) => {
            tracing::debug!(error = %e, "beacon local claim failed");
        }
    }
}

/// Spawn the command-poll loop: when paired, GET + dispatch + ACK every 5 s.
/// The plugin supervisor is shared behind a mutex (its lifecycle methods take
/// `&mut self`) and driven from the blocking pool so the install download and
/// archive unpack never run on the async reactor.
pub(super) fn spawn_command_poll(
    config: Arc<CloudConfig>,
    http: Arc<reqwest::Client>,
    convex_url: String,
    supervisor: SharedSupervisor,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    // A blocking client for the install download (the supervisor install path is
    // synchronous; the download seam is blocking). Built once and reused.
    let download: SharedDownload = Arc::new(ados_plugin_host::download::HttpDownloadSource::new());
    tokio::spawn(async move {
        // Loaded once; every execution persists, so a restart keeps the record.
        let mut executed = dispatch::executed_commands::ExecutedCommands::load(
            std::path::Path::new(dispatch::executed_commands::EXECUTED_COMMANDS_PATH),
        );
        let mut tick = tokio::time::interval(command_poll::POLL_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    let pairing = PairingState::load();
                    let api_key = pairing.api_key();
                    if !should_emit(api_key, &convex_url) {
                        continue;
                    }
                    let api_key = api_key.expect("should_emit gates on api_key being Some");
                    poll_commands_once(
                        &http,
                        &convex_url,
                        api_key,
                        &config.agent.device_id,
                        &supervisor,
                        &download,
                        &mut executed,
                    )
                    .await;
                }
            }
        }
    })
}

/// One command-poll pass over HTTPS ([`command_poll::poll_once`]): GET the
/// queue, dispatch each command for real, ACK the real result. Plugin lifecycle
/// commands run in-process against the held supervisor;
/// service/peripheral/fleet/log/WFB-pair commands forward to the local API over
/// loopback and carry back the route's real ok/failed result; any command with
/// no handler acks an honest `failed("not implemented: …")` rather than
/// fabricating success.
pub(super) async fn poll_commands_once(
    http: &reqwest::Client,
    convex_url: &str,
    api_key: &str,
    device_id: &str,
    supervisor: &SharedSupervisor,
    download: &SharedDownload,
    executed: &mut dispatch::executed_commands::ExecutedCommands,
) {
    let base = convex_url.trim_end_matches('/');
    let link = HttpCommandLink {
        http,
        claim_url: format!("{base}/agent/commands"),
        ack_url: format!("{base}/agent/commands/ack"),
        api_key,
        device_id,
    };
    let runner = LocalRunner {
        http,
        supervisor,
        download,
    };
    command_poll::poll_once(&link, &runner, device_id, executed).await;
}

/// The cloud command queue over HTTPS, authenticated by `X-ADOS-Key`.
pub(super) struct HttpCommandLink<'a> {
    http: &'a reqwest::Client,
    claim_url: String,
    ack_url: String,
    api_key: &'a str,
    device_id: &'a str,
}

impl command_poll::CommandLink for HttpCommandLink<'_> {
    async fn claim(&self) -> Option<serde_json::Value> {
        let resp = self
            .http
            .get(&self.claim_url)
            .query(&[("deviceId", self.device_id)])
            .header("X-ADOS-Key", self.api_key)
            .send()
            .await
            .ok()
            .filter(|r| r.status().is_success())?;
        resp.json().await.ok()
    }

    async fn ack(&self, ack: &serde_json::Value) -> command_poll::AckAttempt {
        match self
            .http
            .post(&self.ack_url)
            .header("X-ADOS-Key", self.api_key)
            .json(ack)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => command_poll::AckAttempt::Delivered,
            Ok(r) => command_poll::AckAttempt::Refused(r.status().as_u16()),
            Err(e) => {
                tracing::debug!(error = %e, "cloud command ack failed");
                command_poll::AckAttempt::Unanswered
            }
        }
    }
}

/// The node's real command handlers ([`dispatch_command`]).
pub(super) struct LocalRunner<'a> {
    http: &'a reqwest::Client,
    supervisor: &'a SharedSupervisor,
    download: &'a SharedDownload,
}

impl command_poll::CommandRunner for LocalRunner<'_> {
    async fn run(&self, name: &str, cmd: &serde_json::Value) -> dispatch::CommandResult {
        let seen = dispatch::seen_jobs::default_path();
        dispatch_command(self.http, name, cmd, self.supervisor, self.download, &seen).await
    }
}

/// Dispatch a single cloud command to its real handler and return the result.
///
/// - Plugin lifecycle commands (`plugin.*`) run in-process against the held
///   supervisor under the mutex (install downloads + `install_archive`; the
///   others enable / disable / uninstall / configure).
/// - Service / peripheral / fleet / log / WFB-pair / raw-command commands map to
///   a local API route and forward over loopback, returning the route's result.
/// - Anything else acks an honest `failed("not implemented: …")`.
pub(super) async fn dispatch_command(
    http: &reqwest::Client,
    name: &str,
    cmd: &serde_json::Value,
    supervisor: &SharedSupervisor,
    download: &SharedDownload,
    seen: &std::path::Path,
) -> dispatch::CommandResult {
    use dispatch::{loopback, plugin_commands};

    // ── Plugin lifecycle: in-process against the held supervisor ──
    // The supervisor ops are synchronous and the install path does a blocking
    // download + archive unpack + `systemctl`, so the whole branch runs on the
    // blocking pool — never on the async reactor.
    if plugin_commands::is_plugin_command(name) {
        let name = name.to_string();
        let cmd = cmd.clone();
        let supervisor = supervisor.clone();
        let download = download.clone();
        let seen = seen.to_path_buf();
        let outcome = tokio::task::spawn_blocking(move || {
            dispatch_plugin_blocking(&name, &cmd, &supervisor, download.as_ref(), &seen)
        })
        .await;
        return match outcome {
            Ok(result) => result,
            Err(e) => dispatch::CommandResult::failed(format!("plugin task panicked: {e}")),
        };
    }

    // ── Loopback to the local API for the work that lives there ──
    let args = cmd.get("args").cloned().unwrap_or(serde_json::Value::Null);
    if let Some(route) = loopback::route_for(name, &args) {
        return loopback::forward(http, name, &args, &route).await;
    }

    // ── No handler: ack an honest failure, never a fabricated success ──
    dispatch::CommandResult::failed(format!("not implemented: {name}"))
}

/// Run a plugin lifecycle command against the held supervisor. Synchronous
/// (filesystem + blocking download + `systemctl`); called from `spawn_blocking`.
pub(super) fn dispatch_plugin_blocking(
    name: &str,
    cmd: &serde_json::Value,
    supervisor: &SharedSupervisor,
    download: &dyn DownloadSource,
    seen: &std::path::Path,
) -> dispatch::CommandResult {
    use dispatch::{install, plugin_commands};

    let mut sup = match supervisor.lock() {
        Ok(g) => g,
        // A poisoned lock means a prior dispatch panicked mid-op; recover the
        // guard and continue rather than crash the relay.
        Err(poisoned) => poisoned.into_inner(),
    };
    if name == "plugin.install" {
        let install_cmd = install::InstallCommand::from_row(cmd);
        return install::handle_install(&mut sup, &install_cmd, download, seen);
    }
    match plugin_commands::PluginCommand::from_row(cmd) {
        Some(pc) => plugin_commands::dispatch(&mut sup, &pc, seen),
        None => dispatch::CommandResult::failed(format!("malformed plugin command: {name}")),
    }
}
