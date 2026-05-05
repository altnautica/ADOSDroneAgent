// ados-agent-lite — main binary entry point.
//
// Single-process, single static binary. Cooperating tokio tasks run the
// MAVLink router, the cloud relay client, and a tiny axum HTTP server
// that exposes /api/v1/setup/status. Reads /etc/ados/agent.yaml at
// startup. Behavior dispatches per the detected board profile loaded
// from /opt/ados/hal/boards/<id>.yaml.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ados_cloud::CloudConfig;
use ados_mavlink::MavlinkConfig;
use ados_setup::{
    setup_router_with_origin_check_and_diag, state::StateStore, DiagState, OriginAllowlist,
    SetupState,
};
use anyhow::{Context, Result};
use axum::extract::DefaultBodyLimit;
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::Value;

#[derive(Parser, Debug)]
#[command(name = "ados-agent-lite")]
#[command(about = "Lightweight ADOS Drone Agent for low-RAM SBCs")]
#[command(version)]
struct Cli {
    /// Path to the agent configuration file.
    #[arg(long, default_value = "/etc/ados/agent.yaml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the agent (default when invoked without a subcommand).
    Run,

    /// Print agent status and exit. Reads from the running agent's HTTP
    /// API at the bind address configured in agent.yaml.
    Status {
        /// Emit JSON to stdout instead of plain-text.
        #[arg(long, short = 'j')]
        json: bool,
    },

    /// Persist a pair code into pairing.json and signal the running
    /// agent to reload. After this the cloud client switches from the
    /// unpaired pairing-beacon flow to the paired heartbeat flow.
    ///
    /// Two modes:
    ///   `pair <CODE>`   — operator provides the code (typed from
    ///                     Mission Control "Add drone").
    ///   `pair --autogen` — mint-or-return the device's current code
    ///                     via PairingStore::get_or_create_code() and
    ///                     print it to stdout. Used by the first-boot
    ///                     surface so a freshly-flashed image emits a
    ///                     code on UART/OLED without operator input.
    Pair {
        /// Pair code from Mission Control "Add drone". Mutually
        /// exclusive with `--autogen`.
        code: Option<String>,
        /// Mint-or-return the device's current pair code via the
        /// canonical TTL semantics and print it. Mutually exclusive
        /// with a positional code.
        #[arg(long, conflicts_with = "code")]
        autogen: bool,
    },

    /// Re-run the install script in upgrade mode. Pulls the latest
    /// signed binary from GitHub Releases, verifies SHA256, and replaces
    /// the on-disk binary in place. Setup state + pairing state are
    /// preserved. Mirrors the universal four-command `ados update`
    /// contract.
    Update {
        /// Check for updates without installing.
        #[arg(long)]
        check_only: bool,
        /// Install without interactive confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Emit JSON result to stdout.
        #[arg(long)]
        json: bool,
        /// Require the fetched install script to match this SHA256 hex
        /// digest before executing it. When omitted, the agent fetches
        /// the script, logs its hash at INFO level for out-of-band
        /// verification, and proceeds. When set, a hash mismatch aborts
        /// the upgrade. Operators who want strict verification on every
        /// run should wire this flag into their orchestration.
        #[arg(long, value_name = "HEX")]
        require_script_sha256: Option<String>,
    },

    /// Stop the agent service, remove the binary + init unit, and
    /// preserve config + pairing state for a possible re-install.
    /// Mirrors the universal four-command `ados uninstall` contract.
    Uninstall {
        /// Also remove the config directory `/etc/ados/`. Without this,
        /// pairing state survives so a subsequent re-install picks up
        /// the same identity.
        #[arg(long)]
        purge: bool,
        /// Skip confirmation prompts.
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Print version information and exit.
    Version,

    /// Validate the agent configuration file and exit. Useful before
    /// restarting the service after editing `agent.yaml` on a live
    /// drone. Checks YAML parse, the api.bind socket address, the
    /// MAVLink port path, and the cloud.convex_url scheme. Exits 0 when
    /// every check passes and 1 when any check fails.
    Validate {
        /// Path to the configuration file to validate. Defaults to the
        /// top-level `--config` flag (which itself defaults to
        /// `/etc/ados/agent.yaml`).
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// Top-level agent configuration loaded from agent.yaml.
#[derive(Debug, Clone, Deserialize)]
struct AgentConfig {
    #[serde(default)]
    agent: AgentSection,
    #[serde(default)]
    mavlink: MavlinkSection,
    #[serde(default)]
    cloud: CloudSection,
    #[serde(default)]
    api: ApiSection,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct AgentSection {
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    #[allow(dead_code)] // Surfaced via /api/v1/setup/status in a later phase.
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct MavlinkSection {
    #[serde(default = "default_mavlink_port")]
    port: String,
    #[serde(default = "default_mavlink_baud")]
    baud: u32,
}

impl Default for MavlinkSection {
    fn default() -> Self {
        Self {
            port: default_mavlink_port(),
            baud: default_mavlink_baud(),
        }
    }
}

/// Manual `Debug` impl below redacts `api_key` so a future
/// `tracing::debug!(?cfg.cloud)` cannot leak the cloud relay bearer
/// into journalctl. The field stays deserializable for back-compat
/// migration off pre-2026-05-05 agent.yaml shapes; only the format
/// trait is custom.
#[derive(Clone, Deserialize)]
struct CloudSection {
    #[serde(default)]
    mqtt_broker: String,
    #[serde(default = "default_mqtt_port")]
    mqtt_port: u16,
    #[serde(default = "default_true")]
    mqtt_use_tls: bool,
    #[serde(default)]
    convex_url: String,
    /// Pre-2026-05-05 versions of agent.yaml stored the per-device API
    /// key here. The canonical location is now /etc/ados/pairing.json
    /// (matching the Python full agent's PairingManager). The field is
    /// retained as deserializable for back-compat — if an old agent.yaml
    /// has it set and pairing.json is empty, we migrate it on first
    /// boot. Going forward, all pair operations write to pairing.json.
    #[serde(default)]
    api_key: String,
    /// HTTP connect-phase timeout in seconds. Operator-tunable so a
    /// half-open TCP doesn't burn the full request budget. Default 3 s.
    #[serde(default = "default_connect_timeout_secs")]
    connect_timeout_secs: u64,
    /// HTTP total request timeout in seconds. Default 10 s.
    #[serde(default = "default_request_timeout_secs")]
    request_timeout_secs: u64,
    /// MQTT keepalive interval in seconds. Operator-tunable so cellular
    /// links can stretch the radio-on cycle. Default 60 s.
    #[serde(default = "default_mqtt_keepalive_secs")]
    mqtt_keepalive_secs: u64,
}

fn default_connect_timeout_secs() -> u64 { 3 }
fn default_request_timeout_secs() -> u64 { 10 }
fn default_mqtt_keepalive_secs() -> u64 { 60 }

impl std::fmt::Debug for CloudSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudSection")
            .field("mqtt_broker", &self.mqtt_broker)
            .field("mqtt_port", &self.mqtt_port)
            .field("mqtt_use_tls", &self.mqtt_use_tls)
            .field("convex_url", &self.convex_url)
            .field(
                "api_key",
                &format_args!("<redacted; {} chars>", self.api_key.len()),
            )
            .finish()
    }
}

impl Default for CloudSection {
    fn default() -> Self {
        Self {
            mqtt_broker: String::new(),
            mqtt_port: default_mqtt_port(),
            mqtt_use_tls: default_true(),
            convex_url: String::new(),
            api_key: String::new(),
            connect_timeout_secs: default_connect_timeout_secs(),
            request_timeout_secs: default_request_timeout_secs(),
            mqtt_keepalive_secs: default_mqtt_keepalive_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ApiSection {
    #[serde(default = "default_api_bind")]
    bind: String,
}

impl Default for ApiSection {
    fn default() -> Self {
        Self {
            bind: default_api_bind(),
        }
    }
}

fn default_mavlink_port() -> String {
    "/dev/ttyS0".into()
}
fn default_mavlink_baud() -> u32 {
    115_200
}
fn default_mqtt_port() -> u16 {
    8883
}
fn default_true() -> bool {
    true
}
fn default_api_bind() -> String {
    // Bind to localhost by default. Operators who need LAN access for the
    // setup webapp must explicitly opt in via api.bind in agent.yaml. This
    // avoids unintentionally exposing the setup surface to other devices
    // on the same Wi-Fi.
    "127.0.0.1:8080".into()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(cli.config).await,
        Command::Status { json } => print_status(&cli.config, json).await,
        Command::Pair { code, autogen } => run_pair(&cli.config, code, autogen).await,
        Command::Update {
            check_only,
            yes,
            json,
            require_script_sha256,
        } => run_update(check_only, yes, json, require_script_sha256).await,
        Command::Uninstall { purge, yes } => run_uninstall(purge, yes).await,
        Command::Version => {
            println!("ados-agent-lite {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Validate { config } => {
            let target = config.unwrap_or(cli.config);
            let ok = run_validate_config(&target);
            if ok {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
    }
}

/// Validate `agent.yaml` and report results to stdout. Returns `true`
/// when every check passed, `false` otherwise. Print is one line per
/// check with a `[ok]` / `[fail]` prefix so the output is grep-friendly
/// and the operator can see which check failed at a glance.
fn run_validate_config(path: &std::path::Path) -> bool {
    let mut ok = true;

    // 1. File exists + is readable. A missing file is the most common
    //    operator mistake (typo in the path), so we check it first and
    //    skip the rest if absent.
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => {
            println!("[ok]   read {}", path.display());
            s
        }
        Err(e) => {
            println!("[fail] read {}: {}", path.display(), e);
            return false;
        }
    };

    // 2. YAML parses into AgentConfig.
    let config: AgentConfig = match serde_yaml::from_str(&raw) {
        Ok(c) => {
            println!("[ok]   parse YAML");
            c
        }
        Err(e) => {
            println!("[fail] parse YAML: {}", e);
            return false;
        }
    };

    // 3. api.bind parses as a SocketAddr.
    match config.api.bind.parse::<SocketAddr>() {
        Ok(_) => println!("[ok]   api.bind ({})", config.api.bind),
        Err(e) => {
            println!("[fail] api.bind ({}): {}", config.api.bind, e);
            ok = false;
        }
    }

    // 4. mavlink.port exists as a filesystem device. The agent itself
    //    opens it later — here we just confirm the path is present so a
    //    typo does not silently fall through.
    if config.mavlink.port.is_empty() {
        println!("[fail] mavlink.port is empty");
        ok = false;
    } else if std::path::Path::new(&config.mavlink.port).exists() {
        println!("[ok]   mavlink.port ({})", config.mavlink.port);
    } else {
        println!(
            "[fail] mavlink.port ({}): no such device",
            config.mavlink.port
        );
        ok = false;
    }

    // 5. cloud.convex_url scheme — empty is allowed (offline mode), but
    //    when set it must be http:// or https://. Operators sometimes
    //    paste the host without the scheme; surface that early.
    if config.cloud.convex_url.is_empty() {
        println!("[ok]   cloud.convex_url (unset; offline mode)");
    } else if config.cloud.convex_url.starts_with("http://")
        || config.cloud.convex_url.starts_with("https://")
    {
        println!("[ok]   cloud.convex_url ({})", config.cloud.convex_url);
    } else {
        println!(
            "[fail] cloud.convex_url ({}): must start with http:// or https://",
            config.cloud.convex_url
        );
        ok = false;
    }

    if ok {
        println!("validation passed");
    } else {
        println!("validation failed");
    }
    ok
}

async fn run_update(
    check_only: bool,
    yes: bool,
    json: bool,
    require_script_sha256: Option<String>,
) -> Result<()> {
    // The lite agent's update path is a thin wrapper around
    // install-lite.sh because it has no in-process OTA channel — the
    // signed binary download and SHA256 verification live in the install
    // script. The four-command contract still requires us to honor
    // --check-only, --yes, and --json so operator UX is identical
    // across agent flavors.
    if check_only {
        // Print the currently installed version and the latest published
        // release tag without invoking the upgrade body.
        let current = env!("CARGO_PKG_VERSION");
        if json {
            let body = serde_json::json!({
                "current": { "version": current, "channel": "stable" },
                "check": { "available": null, "notes": "check-only run; ask GitHub Releases for the live tag" },
            });
            println!("{}", serde_json::to_string_pretty(&body)?);
        } else {
            println!("Installed: ados-agent-lite {current}");
            println!("Run `ados-agent-lite update --yes` to upgrade.");
        }
        return Ok(());
    }
    if !yes && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // Interactive confirmation. Per the contract: prompt once,
        // accept y/Y, abort on anything else.
        eprint!("Install latest version now? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim(), "y" | "Y" | "yes" | "YES") {
            return Ok(());
        }
    }
    run_install_script(&["--upgrade"], require_script_sha256.as_deref()).await
}

async fn run_uninstall(purge: bool, yes: bool) -> Result<()> {
    if !yes && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprint!("Stop and remove ados-agent-lite? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim(), "y" | "Y" | "yes" | "YES") {
            return Ok(());
        }
        if purge {
            eprint!("Also wipe /etc/ados/ (pairing state, secrets)? [y/N] ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            if !matches!(input.trim(), "y" | "Y" | "yes" | "YES") {
                // User said yes to uninstall but no to purge; downgrade.
                return run_install_script(&["--uninstall"], None).await;
            }
        }
    }
    if purge {
        run_install_script(&["--uninstall", "--purge"], None).await
    } else {
        run_install_script(&["--uninstall"], None).await
    }
}

/// Re-run install-lite.sh from the canonical raw URL with the supplied
/// flags. Used by `update` and `uninstall` so the agent stays a single
/// signed static binary — operator state lives in /etc/ados/, not in
/// the agent process. Falls back to a local copy at /usr/local/bin/
/// install-lite.sh when present (developer override).
///
/// Integrity story: the binary that the install script downloads is
/// already SHA256-verified + minisign-checked inside the script itself.
/// The script ITSELF, however, is the bootstrap that decides which URL
/// to fetch and which signature to trust. A hostile substitution at the
/// raw.githubusercontent.com edge (or, more realistically, on the
/// network path of a misconfigured operator with a transparent proxy)
/// could swap the script for one that bypasses signature checks.
///
/// We close that gap by fetching the script to a tempfile, computing its
/// SHA256, and logging the digest at INFO level so an operator can
/// compare against the canonical hash out-of-band. When `expected_sha256`
/// is supplied (via `--require-script-sha256`), a mismatch aborts the
/// run before the script is executed. `ADOS_LITE_ALLOW_UNSIGNED=1`
/// bypasses the requirement for offline-test scenarios.
///
/// Local copies under `/usr/local/share/ados/install-lite.sh` or
/// `/usr/local/bin/install-lite.sh` (developer override) skip the fetch
/// path entirely and therefore the network-substitution threat does not
/// apply; we still hash + log them so a tampered local copy is visible
/// in journalctl.
async fn run_install_script(args: &[&str], expected_sha256: Option<&str>) -> Result<()> {
    use std::process::Command as PCommand;
    const URL: &str =
        "https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install-lite.sh";

    // Decide where the script body comes from. Local override wins; this
    // is what developers use when iterating on the install script itself
    // without pushing to GitHub.
    let local_paths = [
        "/usr/local/share/ados/install-lite.sh",
        "/usr/local/bin/install-lite.sh",
    ];
    let local_override = local_paths
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|p| std::path::PathBuf::from(*p));

    // Land the script at a deterministic tempfile path so a panic mid-run
    // leaves a forensics artifact for `journalctl` + `ls /tmp/ados-*`.
    // tempfile crate is dev-only; std::env::temp_dir + a per-pid suffix
    // keeps the runtime dep tree small. The atomic-write helper does
    // O_CREAT|O_EXCL with mode-at-create so a pre-existing symlink at
    // this path cannot be followed into a privileged write target.
    let tmp = std::env::temp_dir().join(format!("ados-install-lite-{}.sh", std::process::id()));

    // RAII guard: on the happy path we explicitly disarm the guard
    // before successful exec, and on the explicit forensic-retention
    // path (script execution failure) we also disarm so the operator
    // can inspect the tempfile via journalctl. Any other early-exit
    // path (panic, ?, ctrl_c) hits Drop and removes the tempfile.
    let mut tmp_guard = TempfileGuard::new(tmp.clone());

    let script_path: std::path::PathBuf = if let Some(local) = local_override {
        // Copy the local override into the tempfile path. We can't use
        // fs::copy because it would create the destination at the
        // umask-default mode and follow a pre-existing symlink. Read
        // the source bytes and route them through atomic_write at 0o700
        // so the tempfile is created securely with executable mode.
        let bytes = std::fs::read(&local)
            .with_context(|| format!("reading local override {}", local.display()))?;
        ados_setup::atomic::atomic_write(&tmp, &bytes, 0o700)
            .with_context(|| format!("writing local override to {}", tmp.display()))?;
        tracing::info!(
            source = %local.display(),
            "using local install-lite.sh override"
        );
        tmp.clone()
    } else {
        fetch_install_script(URL, &tmp).await?;
        tracing::info!(url = URL, "fetched install-lite.sh");
        tmp.clone()
    };

    // Hash the script and surface the digest so operators can verify the
    // canonical hash out-of-band against the published release.
    let actual_hash = sha256_file(&script_path)
        .with_context(|| format!("hashing {}", script_path.display()))?;
    tracing::info!(
        sha256 = %actual_hash,
        path = %script_path.display(),
        "install-lite.sh sha256 (verify out-of-band against the published release)"
    );

    // Strict verification: when --require-script-sha256 is set we abort
    // on mismatch unless the offline-test bypass is in effect. The bypass
    // mirrors ADOS_CLOUDFLARED_SKIP_SHA256 in the cloudflare module so
    // operator muscle memory carries over.
    if let Some(expected) = expected_sha256 {
        let expected_norm = expected.trim().to_lowercase();
        if std::env::var_os("ADOS_LITE_ALLOW_UNSIGNED").is_some() {
            tracing::warn!(
                expected = %expected_norm,
                actual = %actual_hash,
                "ADOS_LITE_ALLOW_UNSIGNED set; skipping install-lite.sh sha256 enforcement"
            );
        } else if actual_hash != expected_norm {
            // Hostile script body: the RAII guard will unlink the tempfile
            // on the bail!() unwind so it cannot sit around for a later
            // accidental exec. Nothing to do here beyond surfacing the
            // mismatch.
            anyhow::bail!(
                "install-lite.sh sha256 mismatch: expected {}, got {} \
                 (set ADOS_LITE_ALLOW_UNSIGNED=1 to bypass for offline-test)",
                expected_norm,
                actual_hash
            );
        } else {
            tracing::info!(sha256 = %actual_hash, "install-lite.sh sha256 verified");
        }
    }

    // Execute the script body via `sh <path> <args>`. We deliberately do
    // NOT use the curl-pipe pattern any more — the script lives on disk,
    // so we get the same execution semantics with a verifiable artifact.
    //
    // Defense-in-depth: resolve `sh` to an absolute path so a subverted
    // `$PATH` cannot redirect to (e.g.) `/tmp/bin/sh`. The lite agent
    // runs as root on Linux SBCs; `/bin/sh` is universally present. We
    // also try a couple of fallback locations for unusual rootfs layouts
    // before giving up rather than letting `Command::new("sh")` apply
    // PATH-search semantics.
    let sh_path = resolve_sh_binary()
        .context("no sh interpreter found at /bin/sh, /usr/bin/sh, or /system/bin/sh")?;
    let mut command = PCommand::new(sh_path);
    command.arg(&script_path).args(args);
    let status = command
        .status()
        .with_context(|| format!("running install-lite.sh {}", args.join(" ")))?;

    // Clean up the tempfile on success. On failure we leave it in place
    // so the operator can inspect what ran — disarm the guard so Drop
    // does NOT remove it, then return the failure.
    if status.success() {
        // Happy-path cleanup: remove the file proactively, then disarm
        // the guard so Drop is a no-op. (Disarming first then removing
        // would also work but this order makes the cleanup explicit.)
        let _ = std::fs::remove_file(&script_path);
        tmp_guard.disarm();
        Ok(())
    } else {
        tracing::warn!(
            path = %script_path.display(),
            "install-lite.sh failed; tempfile retained for inspection"
        );
        // Forensic-retention path: keep the tempfile on disk so the
        // operator can inspect the failing body.
        tmp_guard.disarm();
        anyhow::bail!("install-lite.sh exited with code {:?}", status.code());
    }
}

/// RAII guard that unlinks a tempfile on Drop unless explicitly disarmed.
///
/// Mirrors the AbortOnDrop pattern used elsewhere in the agent: the
/// happy path (and any explicit retention path) calls `disarm()` so
/// Drop is a no-op; every other early-exit path — panic, `?`-bubbled
/// error between fetch and exec, ctrl_c — falls through Drop and the
/// tempfile gets cleaned up. This closes the leak window where a
/// fetched script body would otherwise persist on disk after a fault.
struct TempfileGuard {
    path: Option<std::path::PathBuf>,
}

impl TempfileGuard {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Disarm the guard so Drop becomes a no-op. Used when the caller
    /// has either already cleaned up the tempfile or wants to retain
    /// it deliberately (forensic inspection on script failure).
    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempfileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Fetch the install script over HTTPS using reqwest with rustls. We
/// avoid shelling out to curl/wget here so the body lands directly in a
/// Rust-controlled buffer (no shell-pipe race window between fetch and
/// hash).
async fn fetch_install_script(url: &str, dest: &std::path::Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .with_context(|| "building reqwest client for install-lite.sh fetch")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("fetching {url} returned HTTP {}", resp.status());
    }
    let body = resp
        .bytes()
        .await
        .with_context(|| format!("reading body of {url}"))?;
    // Route through the atomic-write helper so the tempfile is created
    // with O_CREAT|O_EXCL and 0o700 mode-at-create. A pre-existing
    // symlink at the destination causes EEXIST rather than a follow.
    ados_setup::atomic::atomic_write(dest, &body, 0o700)
        .with_context(|| format!("writing install-lite.sh to {}", dest.display()))?;
    Ok(())
}

/// Stream the file through SHA256 and return the lowercase hex digest.
/// Mirrors the helper in `crates/ados-setup/src/cloudflare.rs` so the
/// agent has a single internal convention for binary integrity hashes.
fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    Ok(out)
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).compact().init();
}

async fn print_status(config_path: &std::path::Path, as_json: bool) -> Result<()> {
    let config = load_config(config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    // Construct the status URL from the configured bind address. When the
    // agent binds to 0.0.0.0 the CLI still reaches it via 127.0.0.1.
    let bind_addr: SocketAddr = config
        .api
        .bind
        .parse()
        .with_context(|| format!("invalid api.bind address: {}", config.api.bind))?;
    let host = if bind_addr.ip().is_unspecified() {
        std::net::IpAddr::from([127u8, 0, 0, 1])
    } else {
        bind_addr.ip()
    };
    let url = format!("http://{}:{}/api/v1/setup/status", host, bind_addr.port());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("contacting agent at {}", url))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .with_context(|| "parsing agent status response")?;

    if as_json {
        println!("{}", serde_json::to_string_pretty(&body)?);
    } else {
        let device_id = body
            .get("device_id")
            .and_then(|v| v.as_str())
            .unwrap_or("<unset>");
        let version = body.get("version").and_then(|v| v.as_str()).unwrap_or("?");
        let runtime_mode = body
            .get("runtime_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let profile = body.get("profile").and_then(|v| v.as_str()).unwrap_or("?");
        let setup_finalized = body
            .get("setup_finalized")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        println!("ados-agent-lite {} (HTTP {})", version, status.as_u16());
        println!("  device_id:        {}", device_id);
        println!("  profile:          {}", profile);
        println!("  runtime_mode:    {}", runtime_mode);
        println!("  setup_finalized: {}", setup_finalized);
    }
    Ok(())
}

async fn run(config_path: PathBuf) -> Result<()> {
    let config = load_config(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        device_id = config.agent.device_id,
        "ados-agent-lite starting"
    );

    // Cooperating tasks. Each spawns its own background work. The main
    // task waits for shutdown signal and supervises panics via Tokio's
    // catch-unwind in spawn().

    // Detect board metadata once at startup. Heartbeat enrichment uses
    // these values verbatim; re-running the probe per heartbeat tick
    // would cost ~3 ms × every 5 s for no gain (board hardware does not
    // change at runtime). Network identity is re-detected per tick
    // inside the cloud client because DHCP renewals can flip lastIp.
    let board_meta = ados_setup::hardware::detect_board_metadata();

    // Resolve pairing.json path. Defaults to /etc/ados/pairing.json (next
    // to agent.yaml). Tests + dev containers override via the env var.
    let pairing_path = std::env::var_os("ADOS_PAIRING_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("/etc/ados"))
                .join("pairing.json")
        });

    // Allow override via ADOS_SETUP_STATE_PATH so tests + dev containers
    // don't need /var write access. Production install puts this at
    // /var/lib/ados/setup/state.json — same path the Python full agent
    // uses, so an operator can swap between agents without losing setup
    // state.
    let setup_state_path = std::env::var_os("ADOS_SETUP_STATE_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/ados/setup/state.json"));

    // One greppable line that names every config decision the agent made
    // at boot. An operator opening journalctl after `systemctl restart
    // ados-agent-lite` can correlate the running config (broker, relay
    // URL, paths, board) without dumping individual debug lines. No
    // secrets — pair codes, api keys, and tokens never land here.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        device_id = %config.agent.device_id,
        mqtt_broker = %if config.cloud.mqtt_broker.is_empty() {
            "<none>".to_string()
        } else {
            config.cloud.mqtt_broker.clone()
        },
        mqtt_port = config.cloud.mqtt_port,
        mqtt_use_tls = config.cloud.mqtt_use_tls,
        convex_url = %if config.cloud.convex_url.is_empty() {
            "<none>".to_string()
        } else {
            config.cloud.convex_url.clone()
        },
        pairing_path = %pairing_path.display(),
        setup_state_path = %setup_state_path.display(),
        agent_yaml = %config_path.display(),
        bind_addr = %config.api.bind,
        mavlink_port = %config.mavlink.port,
        mavlink_baud = config.mavlink.baud,
        board_name = %board_meta.board_name.as_deref().unwrap_or("<unknown>"),
        soc = %board_meta.soc.as_deref().unwrap_or("<unknown>"),
        arch = %board_meta.arch.as_deref().unwrap_or("<unknown>"),
        ram_mb = ?board_meta.ram_mb,
        "boot configuration"
    );

    let mavlink_config = MavlinkConfig {
        port: config.mavlink.port.clone(),
        baud: config.mavlink.baud,
    };

    let mavlink_handles = match ados_mavlink::spawn_router(mavlink_config) {
        Ok(handles) => Some(handles),
        Err(e) => {
            // FC serial is not always present in dev / cgroup tests. Log
            // and continue so the cloud heartbeat path can still surface
            // the agent in the fleet view.
            tracing::warn!(error = %e, "mavlink router unavailable; continuing without FC link");
            None
        }
    };

    // Migrate legacy agent.yaml `cloud.api_key` to pairing.json on first
    // boot if the new file is empty. This preserves operator pairings
    // from pre-2026-05-05 agent.yaml configs without forcing a re-pair.
    //
    // The legacy field carried a `ados_<base64url>` per-device key, NOT
    // an operator-typed pair code, so we route through `migrate_legacy_api_key`
    // (which preserves byte-exact case) rather than `set_code` (which
    // uppercases for operator typo tolerance and would corrupt the key).
    if !config.cloud.api_key.is_empty() {
        let store = ados_setup::pairing::PairingStore::new(&pairing_path);
        if let Ok(existing) = store.load() {
            if !existing.is_paired() {
                tracing::info!(
                    "migrating legacy cloud.api_key from agent.yaml -> pairing.json"
                );
                match store.migrate_legacy_api_key(&config.cloud.api_key) {
                    Ok(_) => tracing::info!(
                        path = %pairing_path.display(),
                        "migrated legacy cloud.api_key to pairing.json"
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        path = %pairing_path.display(),
                        "legacy cloud.api_key migration failed; pairing.json may be \
                         unwritable (disk full, permission denied, or read-only mount) — \
                         the agent will continue with the unpaired beacon"
                    ),
                }
            }
        }
    }

    let agent_meta = ados_cloud::AgentMeta {
        board_name: board_meta.board_name,
        soc: board_meta.soc,
        arch: board_meta.arch,
        ram_mb: board_meta.ram_mb,
        hostname: Some(sys_hostname()),
        last_ip: sys_local_ips().into_iter().next(),
        mdns_host: Some(format!("ados-{}.local", config.agent.device_id)),
    };

    let cloud_config = CloudConfig {
        device_id: config.agent.device_id.clone(),
        mqtt_broker: config.cloud.mqtt_broker.clone(),
        mqtt_port: config.cloud.mqtt_port,
        mqtt_use_tls: config.cloud.mqtt_use_tls,
        convex_url: config.cloud.convex_url.clone(),
        pairing_path: pairing_path.clone(),
        agent_meta: Some(agent_meta),
        connect_timeout_secs: config.cloud.connect_timeout_secs,
        request_timeout_secs: config.cloud.request_timeout_secs,
        mqtt_keepalive_secs: config.cloud.mqtt_keepalive_secs,
    };

    // Diagnostic state shared across the HTTP handlers and the cloud /
    // MAVLink tasks that record counters. The same Arc clone is handed
    // to the diag-aware router below so the /api/v1/diag handler reads
    // the live counters the cloud client updates.
    let diag_state = DiagState::shared();

    // Spawn the cloud client when we have an identity. Missing MQTT broker
    // is handled inside the cloud client (it skips the MQTT publish loop
    // and runs only the HTTP loop, which serves the unpaired beacon and
    // the paired heartbeat). Missing convex_url is also tolerated — the
    // HTTP loop logs and waits for the operator to configure it via the
    // setup webapp or by editing agent.yaml directly.
    if !cloud_config.device_id.is_empty() {
        let mavlink_inbound = mavlink_handles
            .as_ref()
            .map(|h| h.inbound.clone())
            .unwrap_or_else(|| {
                let (tx, _rx) = tokio::sync::broadcast::channel(16);
                tx
            });
        // FC writer for cloud-received MAVLink frames. Pass through only
        // when a real router is up; otherwise the cloud client logs +
        // drops inbound mavlink/rx publishes.
        let fc_writer = mavlink_handles.as_ref().map(|h| h.outbound.clone());
        if let Err(e) = ados_cloud::spawn_cloud_client(
            cloud_config,
            mavlink_inbound,
            fc_writer,
            diag_state.clone(),
        ) {
            tracing::warn!(error = %e, "cloud client spawn failed; running offline");
        }
    } else {
        tracing::info!("device_id missing; running offline (no mqtt, no heartbeat)");
    }

    // axum HTTP server: full universal setup surface mounted from
    // ados-setup. Status snapshot reads live agent state
    // (paired/unpaired, mavlink port + baud, device_id). The crate-level
    // state owns the agent.yaml path + setup-state.yaml store; this
    // binary supplies the snapshot builder closure.
    let app_state_inner = Arc::new(AppState {
        device_id: config.agent.device_id.clone(),
        mavlink_port: config.mavlink.port.clone(),
        mavlink_baud: config.mavlink.baud,
        pairing_path: pairing_path.clone(),
    });

    let setup_state_store = StateStore::new(setup_state_path);
    let snapshot_state = app_state_inner.clone();
    let snapshot_store = setup_state_store.clone();
    let snapshot_yaml = config_path.clone();
    let setup_state = Arc::new(SetupState {
        agent_yaml: config_path.clone(),
        store: setup_state_store,
        status_builder: Box::new(move || {
            build_setup_status(&snapshot_state, &snapshot_store, &snapshot_yaml)
        }),
    });

    let bind_addr: SocketAddr = config
        .api
        .bind
        .parse()
        .with_context(|| format!("invalid api.bind address: {}", config.api.bind))?;
    if bind_addr.ip().is_unspecified() {
        tracing::warn!(
            addr = %bind_addr,
            "http api binding 0.0.0.0; setup surface is exposed to every interface"
        );
    }

    // Build the same-origin allowlist for the setup REST surface. Read
    // methods + curl-style header-less requests pass through; only
    // cross-origin POST / PUT / PATCH / DELETE requests are rejected.
    // This is defense-in-depth for the common operator path of binding
    // to 0.0.0.0 so a tablet on the same LAN can run the wizard.
    let bind_host_str = bind_addr.ip().to_string();
    let origin_allowlist = Arc::new(OriginAllowlist::new(
        &bind_host_str,
        bind_addr.port(),
        &config.agent.device_id,
    ));
    tracing::info!(
        bind_host = %bind_host_str,
        bind_port = bind_addr.port(),
        device_id = %config.agent.device_id,
        "setup origin allowlist configured"
    );

    // `diag_state` was constructed above (before the cloud client spawn)
    // so the same Arc clone reaches both the cloud relay tasks and the
    // /api/v1/diag handler. The diag handler reads the live counters the
    // cloud relay updates so `mqtt.connected_recently`,
    // `cloud_relay.last_heartbeat_at`, and consecutive-failure counts
    // reflect runtime state.

    let app =
        setup_router_with_origin_check_and_diag(setup_state, origin_allowlist, diag_state.clone())
            // Cap request bodies at 64 KiB. The setup surface accepts no
            // large payloads today; this is defense-in-depth for the POST
            // handlers.
            .layer(DefaultBodyLimit::max(64 * 1024));
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("binding HTTP server on {}", bind_addr))?;
    tracing::info!(addr = %bind_addr, "http api listening");

    let server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "http api exited with error");
        }
    });

    // Wait for shutdown. systemd sends SIGTERM on `systemctl stop`; an
    // operator's terminal sends SIGINT (ctrl_c). Both must surface here
    // so the agent logs the signal it received before unwinding — that
    // single line is what an operator correlates against journalctl when
    // diagnosing "why did the agent restart" later.
    //
    // The cloud client + mavlink router run inside spawned tasks. When
    // this future returns, the Tokio current_thread runtime drops them
    // cooperatively. The eventloop AbortOnDrop guard inside the cloud
    // client cancels the rumqttc poll task immediately on parent drop,
    // so no zombie tasks survive shutdown. Atomic-write helpers fsync
    // before returning, so any pairing.json or setup-state.json write
    // that returned to its caller is durable on disk.
    #[cfg(unix)]
    let shutdown_signal = async {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                // Falling back to ctrl_c only is preferable to crashing
                // the agent on a system that, for whatever reason, has
                // exhausted its signal-handler slots.
                tracing::warn!(error = %e, "could not install SIGTERM handler; falling back to SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
                return "SIGINT".to_string();
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT".to_string(),
            _ = sigterm.recv() => "SIGTERM".to_string(),
        }
    };
    #[cfg(not(unix))]
    let shutdown_signal = async {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT".to_string()
    };

    tokio::select! {
        signal = shutdown_signal => {
            tracing::info!(signal = %signal, "received shutdown signal; cleaning up");
        }
        result = server => {
            tracing::warn!(?result, "http api task ended");
        }
    }

    tracing::info!("shutdown complete");
    Ok(())
}

fn load_config(path: &std::path::Path) -> Result<AgentConfig> {
    if !path.exists() {
        tracing::warn!(path = %path.display(), "config file missing; using defaults");
        return Ok(AgentConfig {
            agent: AgentSection::default(),
            mavlink: MavlinkSection::default(),
            cloud: CloudSection::default(),
            api: ApiSection::default(),
        });
    }
    let raw = std::fs::read_to_string(path)?;
    let parsed: AgentConfig = serde_yaml::from_str(&raw)
        .with_context(|| format!("parsing yaml at {}", path.display()))?;
    Ok(parsed)
}

/// Dispatch the `pair` subcommand: either persist an operator-typed
/// code, or mint-or-return the device's current code via the canonical
/// TTL semantics. The two modes are mutually exclusive at the clap
/// layer; this function enforces the "must pick one" rule and routes
/// to the right helper.
async fn run_pair(
    config_path: &std::path::Path,
    code: Option<String>,
    autogen: bool,
) -> Result<()> {
    match (code, autogen) {
        (Some(code), false) => persist_pair_code(config_path, &code).await,
        (None, true) => autogen_pair_code(config_path).await,
        (None, false) => anyhow::bail!(
            "pair: provide a code or pass --autogen (e.g. `ados-agent-lite pair ABC123` or \
             `ados-agent-lite pair --autogen`)"
        ),
        (Some(_), true) => {
            // clap's `conflicts_with` already rejects this at parse
            // time, but defense-in-depth keeps the invariant local.
            anyhow::bail!("pair: --autogen conflicts with a positional code")
        }
    }
}

/// Mint-or-return the device's current pair code via
/// `PairingStore::get_or_create_code()` and print it. Used by the
/// first-boot surface (S98ados-first-boot) so a freshly-flashed image
/// emits a code on UART/OLED without requiring operator input.
///
/// The output format is the canonical first-boot banner so consumers
/// (the script that sources this output, the operator reading the
/// UART) get a stable, greppable line.
async fn autogen_pair_code(config_path: &std::path::Path) -> Result<()> {
    let pairing_path = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/etc/ados"))
        .join("pairing.json");

    let store = ados_setup::pairing::PairingStore::new(&pairing_path);
    let code = store
        .get_or_create_code()
        .with_context(|| format!("autogen pair code at {}", pairing_path.display()))?;

    // Canonical banner. The first-boot surface greps this for the
    // 6-character code; any change to the format must update
    // package/ados-agent-lite/first-boot.sh in lockstep.
    println!("==== ADOS PAIR CODE: {} ====", code);
    Ok(())
}

/// Persist a pair code via the canonical `pairing.json` path (mirrors
/// the Python full agent's PairingManager). The pair code goes to
/// `pairing.json:pairing_code`, NOT to `agent.yaml:cloud.api_key` —
/// those are different values per the proto:
///
/// - `pairing_code` is a short 6-char operator-readable code that gets
///   typed into Mission Control's "Add drone" dialog.
/// - `api_key` is the long `ados_<base64url-32>` per-device bearer
///   the cloud relay returns AFTER a successful claim.
///
/// Conflating them (the prior implementation) broke the cloud relay's
/// X-ADOS-Key header check on heartbeats — the agent was sending its
/// short pair code as the API key.
///
/// On success this also signals the running service to reload by
/// restarting via systemd / busybox sysv-rc / runit.
async fn persist_pair_code(config_path: &std::path::Path, code: &str) -> Result<()> {
    if code.is_empty() {
        anyhow::bail!("pair code is empty");
    }

    // The pairing.json path lives next to /etc/ados/agent.yaml. We derive
    // it from the config path's parent so test runs (and dev containers)
    // that override the config path automatically pick up a sibling
    // pairing.json without further env wiring.
    let pairing_path = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/etc/ados"))
        .join("pairing.json");

    let store = ados_setup::pairing::PairingStore::new(&pairing_path);
    store
        .set_code(code)
        .with_context(|| format!("writing {}", pairing_path.display()))?;

    println!("pair code saved to {}", pairing_path.display());

    // Best-effort: signal the running service to pick up the new code.
    // Absolute paths so a subverted $PATH does not redirect to a hostile
    // binary. We try systemd first, then busybox sysv-rc, then runit.
    let restart_attempts: &[(&str, &[&str])] = &[
        ("/usr/bin/systemctl", &["restart", "ados-agent-lite.service"]),
        ("/bin/systemctl", &["restart", "ados-agent-lite.service"]),
        ("/etc/init.d/S99ados-agent-lite", &["restart"]),
        ("/usr/bin/sv", &["restart", "ados-agent-lite"]),
    ];
    for (program, args) in restart_attempts {
        if !std::path::Path::new(program).exists() {
            continue;
        }
        match std::process::Command::new(program).args(*args).status() {
            Ok(s) if s.success() => {
                println!("restarted service via {}", program);
                return Ok(());
            }
            _ => continue,
        }
    }
    println!(
        "code saved. Restart the service to pick up the new pair code: \
         sudo systemctl restart ados-agent-lite (systemd) or \
         sudo /etc/init.d/S99ados-agent-lite restart (busybox)"
    );
    Ok(())
}

#[derive(Clone)]
struct AppState {
    device_id: String,
    mavlink_port: String,
    mavlink_baud: u32,
    pairing_path: PathBuf,
}

fn build_setup_status(
    state: &Arc<AppState>,
    store: &StateStore,
    agent_yaml: &std::path::Path,
) -> Value {
    // Returns the canonical SetupStatus shape so consumers (the setup
    // webapp, Mission Control, the cloud relay) read every expected
    // field. Re-reads pairing.json each call so a `ados-agent-lite pair`
    // from another process flips paired-state on the next /status query
    // without needing an agent restart.
    let persisted = store.load().unwrap_or_default();
    let pairing_state = ados_setup::pairing::PairingStore::new(&state.pairing_path)
        .load()
        .unwrap_or_default();
    let paired = pairing_state.is_paired();
    let next_action = if persisted.finalized || paired {
        "ready"
    } else {
        "pair"
    };
    let skipped: Vec<String> = persisted.skipped_steps.iter().cloned().collect();
    // Read everything we can from the live agent.yaml so the wizard
    // reflects operator config + the cloud relay surface drives the
    // GCS Lite-card plumbing correctly.
    let yaml_view = read_yaml_view(agent_yaml);
    let (profile, ground_role) = (yaml_view.profile.clone(), yaml_view.ground_role.clone());
    // Hostname + local IPs for the network block (best-effort; falls
    // back to empties on systems without these probes).
    let hostname = sys_hostname();
    let local_ips = sys_local_ips();
    // api.bind parses to "host:port" — extract the port for the
    // network.api_port surface (falls back to 8080 if parse fails).
    let api_port = yaml_view
        .api_bind
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(8080);

    // Compute the 10-step lifecycle. Mirrors src/ados/setup/service.py
    // so the wizard sidebar surfaces identical step states regardless of
    // which agent half (Python full or Rust lite) is serving.
    let steps = build_steps(
        &profile,
        paired,
        persisted.finalized,
        &persisted.skipped_steps,
    );
    let total_steps = steps.len();
    let complete_count = steps
        .iter()
        .filter(|s| s.get("state").and_then(|v| v.as_str()) == Some("complete"))
        .count();
    let completion_percent = if total_steps == 0 {
        0
    } else {
        ((complete_count as f64 / total_steps as f64) * 100.0).round() as i64
    };
    let next_action = steps
        .iter()
        .find(|s| s.get("state").and_then(|v| v.as_str()) == Some("needs_action"))
        .and_then(|s| s.get("action_label").and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_else(|| if persisted.finalized { "ready".into() } else { next_action.into() });

    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "agent_version": env!("CARGO_PKG_VERSION"),
        "device_id": state.device_id,
        "device_name": yaml_view.agent_name,
        "profile": profile,
        "ground_role": ground_role,
        "runtime_mode": "lite",
        "setup_complete": persisted.finalized || paired,
        "setup_finalized": persisted.finalized,
        "completion_percent": if persisted.finalized { 100 } else { completion_percent },
        "next_action": next_action,
        "steps": steps,
        "skipped_steps": skipped,
        "access_urls": [],
        "network": {
            "hostname": hostname,
            "mdns_host": format!("ados-{}.local", state.device_id),
            "api_port": api_port,
            "hotspot_enabled": false,
            "hotspot_ssid": "",
            "local_ips": local_ips
        },
        "mavlink": {
            "connected": false,
            "port": state.mavlink_port,
            "baud": state.mavlink_baud,
            "websocket_url": null,
            "public_websocket_url": null
        },
        "video": {
            "state": "not_initialized",
            "whep_url": null,
            "public_whep_url": null,
            "recording": false
        },
        "remote_access": {
            "provider": "none",
            "enabled": false,
            "configured": false,
            "status": "disabled",
            "public_urls": [],
            "error": ""
        },
        "cloud_choice": {
            "mode": yaml_view.cloud_mode,
            "paired": paired,
            "pair_code_required": !paired,
            "backend_url": yaml_view.cloud_url,
            "backend_reachable": false,
            "last_checked": null
        },
        "profile_suggestion": {
            "detected": "unconfigured",
            "ground_role_hint": "direct",
            "ground_score": 0,
            "air_score": 0,
            "mesh_capable": false,
            "signals": {},
            "confirmed": false,
            "detected_at": null
        },
        "hardware_check": null,
        "services": [
            { "name": "mavlink-router", "state": "running" },
            { "name": "cloud-client",   "state": "running" },
            { "name": "http-api",       "state": "running" }
        ],
        "telemetry": {}
    })
}

/// Build the 10-step wizard lifecycle list with per-step state.
/// Mirrors `src/ados/setup/service.py:build_setup_status` step-derivation
/// so consumers (the universal setup webapp + Mission Control) render
/// the same progress sidebar regardless of which agent serves.
///
/// At the lite control-plane level we have visibility into:
/// - profile + ground_role (from agent.yaml)
/// - paired (from pairing.json)
/// - finalized + skipped_steps (from setup-state.json)
///
/// We do NOT yet have live mavlink heartbeat / video / remote-access
/// state; those steps mark themselves `needs_action` (or `optional` if
/// the operator skipped them). Once Phase E3 wires runtime state, the
/// derivations below grow.
fn build_steps(
    profile: &str,
    paired: bool,
    finalized: bool,
    skipped: &std::collections::BTreeSet<String>,
) -> Vec<serde_json::Value> {
    let is_drone = profile == "drone";
    let is_ground = profile == "ground_station";
    let mut out: Vec<serde_json::Value> = Vec::new();

    let push = |out: &mut Vec<_>, id: &str, label: &str, state: &str, detail: &str, action_label: &str| {
        let mut effective_state = state.to_string();
        if skipped.contains(id) && state == "needs_action" {
            effective_state = "optional".into();
        }
        out.push(serde_json::json!({
            "id": id,
            "label": label,
            "state": effective_state,
            "detail": detail,
            "action_label": action_label,
            "href": "",
        }));
    };

    // welcome — always complete (the operator made it past the welcome screen).
    push(&mut out, "welcome", "Welcome", "complete", "Onboarding starting.", "");

    // profile — complete when one of {drone, ground_station} is set.
    if is_drone || is_ground {
        push(
            &mut out,
            "profile",
            "Profile",
            "complete",
            &format!("{} profile selected.", if is_drone { "Drone" } else { "Ground station" }),
            "",
        );
    } else {
        push(
            &mut out,
            "profile",
            "Profile",
            "needs_action",
            "Pick the role this device serves.",
            "Choose profile",
        );
    }

    // hardware_check — at lite v1 we treat it as needs_action by default;
    // the operator runs the explicit /hardware-check route to populate it.
    // Real per-component derivation lands in Phase E3 alongside the
    // runtime hardware-check engine.
    push(
        &mut out,
        "hardware_check",
        "Hardware",
        "needs_action",
        "Verify the FC, camera, and Wi-Fi adapters.",
        "Run hardware check",
    );

    // cloud_choice — surfaced after profile. Complete when paired (the
    // pair flow implies the cloud_choice step landed first).
    if paired {
        push(&mut out, "cloud_choice", "Cloud", "complete", "Cloud relay configured.", "");
    } else {
        push(
            &mut out,
            "cloud_choice",
            "Cloud",
            "needs_action",
            "Pick how this device reaches Mission Control.",
            "Choose cloud mode",
        );
    }

    // pair — operator-typed code claim.
    if paired {
        push(&mut out, "pair", "Pair", "complete", "Device claimed.", "");
    } else {
        push(
            &mut out,
            "pair",
            "Pair",
            "needs_action",
            "Enter the pair code from Mission Control.",
            "Pair device",
        );
    }

    // mavlink — drone profile only. Live heartbeat probe lands in Phase E3.
    if is_drone {
        push(
            &mut out,
            "mavlink",
            "Flight controller",
            "needs_action",
            "Connect a flight controller over USB or UART.",
            "Connect FC",
        );
    }

    // video — always emitted; the lite control plane has no video
    // pipeline today, so the step stays needs_action until the encoder
    // backend ships.
    push(
        &mut out,
        "video",
        "Video",
        "needs_action",
        "Video pipeline lands in the next phase.",
        "Configure video",
    );

    // ground_receiver — ground_station profile only.
    if is_ground {
        push(
            &mut out,
            "ground_receiver",
            "Ground receiver",
            "needs_action",
            "Configure WFB-ng receiver dongle.",
            "Configure receiver",
        );
    }

    // remote_access — always optional unless explicitly configured.
    push(
        &mut out,
        "remote_access",
        "Remote access",
        "optional",
        "Add a Cloudflare Tunnel for off-LAN access.",
        "Configure tunnel",
    );

    // finish — complete when finalized.
    if finalized {
        push(&mut out, "finish", "Finish", "complete", "Setup complete.", "");
    } else {
        push(
            &mut out,
            "finish",
            "Finish",
            "needs_action",
            "Confirm setup is complete.",
            "Finish",
        );
    }

    out
}

/// Live view of agent.yaml that the SetupStatus surface needs. Read on
/// every /status call so operator edits to the file flow through to
/// Mission Control on the next heartbeat without an agent restart.
#[derive(Debug, Clone)]
struct YamlView {
    agent_name: String,
    profile: String,
    ground_role: String,
    cloud_mode: String,
    cloud_url: String,
    api_bind: String,
}

impl Default for YamlView {
    fn default() -> Self {
        Self {
            agent_name: "ADOS Lite Agent".into(),
            profile: "drone".into(),
            ground_role: String::new(),
            cloud_mode: "cloud".into(),
            cloud_url: String::new(),
            api_bind: default_api_bind(),
        }
    }
}

fn read_yaml_view(path: &std::path::Path) -> YamlView {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return YamlView::default(),
    };
    let doc: serde_yaml::Value = match serde_yaml::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return YamlView::default(),
    };
    let s = |path: &[&str]| -> Option<String> {
        let mut cur = &doc;
        for k in path {
            cur = cur.get(k)?;
        }
        cur.as_str().map(String::from)
    };
    YamlView {
        agent_name: s(&["agent", "name"]).unwrap_or_else(|| "ADOS Lite Agent".into()),
        profile: s(&["agent", "profile"]).unwrap_or_else(|| "drone".into()),
        ground_role: s(&["ground_station", "role"]).unwrap_or_default(),
        cloud_mode: s(&["cloud", "mode"]).unwrap_or_else(|| "cloud".into()),
        cloud_url: s(&["cloud", "convex_url"]).unwrap_or_default(),
        api_bind: s(&["api", "bind"]).unwrap_or_else(default_api_bind),
    }
}

/// Best-effort hostname read. Falls back to empty string on failure.
fn sys_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .or_else(|| {
            // macOS / non-Linux fallback via the `hostname` shell command.
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default()
}

/// Enumerate non-loopback IPv4 + IPv6 addresses by parsing /sys/class/net.
/// Best-effort; returns empty vec when the sysfs surface is unavailable.
/// We avoid the `nix` crate to keep the dep tree lean — `getifaddrs`
/// would be cleaner but for v1 reading sysfs + /proc/net/fib_trie is
/// pragmatic.
///
/// Defense-in-depth: `ip` is resolved via an absolute-path allowlist
/// (`/usr/sbin/ip`, `/sbin/ip`, `/usr/bin/ip`) so a subverted `$PATH`
/// cannot redirect to a hostile binary. Returns an empty Vec if none of
/// the allowed paths exist rather than falling back to PATH-search
/// semantics.
fn sys_local_ips() -> Vec<String> {
    // Shell out to `ip -4 -o addr show scope global` which exists on
    // every Linux Buildroot rootfs. Resolve the binary via the
    // absolute-path allowlist first so `$PATH` cannot redirect this.
    let Some(ip_bin) = resolve_ip_binary() else {
        return Vec::new();
    };
    let out = std::process::Command::new(ip_bin)
        .args(["-o", "-4", "addr", "show", "scope", "global"])
        .output();
    let mut ips = Vec::new();
    if let Ok(o) = out {
        if let Ok(text) = String::from_utf8(o.stdout) {
            for line in text.lines() {
                // Format: "2: wlan0    inet 192.168.200.225/24 brd ..."
                if let Some(idx) = line.find("inet ") {
                    let rest = &line[idx + 5..];
                    if let Some(end) = rest.find('/') {
                        ips.push(rest[..end].to_string());
                    }
                }
            }
        }
    }
    ips
}

/// Resolve the `sh` interpreter to an absolute path so the script-runner
/// is immune to `$PATH` injection. Tries `/bin/sh` first (universally
/// present on Linux SBCs), then `/usr/bin/sh`, then `/system/bin/sh`
/// (Android-derived rootfs). Returns the first path that exists, or
/// None if none do.
fn resolve_sh_binary() -> Option<&'static str> {
    const CANDIDATES: &[&str] = &["/bin/sh", "/usr/bin/sh", "/system/bin/sh"];
    for candidate in CANDIDATES {
        if std::path::Path::new(candidate).exists() {
            return Some(*candidate);
        }
    }
    None
}

/// Resolve `ip` to an absolute path. Tries `/usr/sbin/ip` (modern
/// Buildroot), `/sbin/ip` (Debian/legacy), `/usr/bin/ip` (some
/// distros). Returns the first path that exists, or None — the caller
/// should treat None as "no local-IP enumeration this tick" rather than
/// fall back to `$PATH` search.
fn resolve_ip_binary() -> Option<&'static str> {
    const CANDIDATES: &[&str] = &["/usr/sbin/ip", "/sbin/ip", "/usr/bin/ip"];
    for candidate in CANDIDATES {
        if std::path::Path::new(candidate).exists() {
            return Some(*candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_defaults_load_from_empty_yaml() {
        let parsed: AgentConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(parsed.mavlink.port, "/dev/ttyS0");
        assert_eq!(parsed.mavlink.baud, 115_200);
        assert_eq!(parsed.cloud.mqtt_port, 8883);
        assert!(parsed.cloud.mqtt_use_tls);
        assert_eq!(parsed.api.bind, "127.0.0.1:8080");
    }

    #[test]
    fn resolve_sh_binary_returns_absolute_path() {
        // /bin/sh is universally present on Linux + macOS dev hosts.
        // The resolver must never return a relative path or None on
        // a system that obviously has it. Catches a future refactor
        // that flips candidate ordering or drops the canonical path.
        let resolved = resolve_sh_binary().expect(
            "expected /bin/sh, /usr/bin/sh, or /system/bin/sh to exist on this host",
        );
        assert!(resolved.starts_with('/'), "sh path must be absolute");
        assert!(
            std::path::Path::new(resolved).exists(),
            "resolved path {resolved} must exist"
        );
    }

    #[test]
    fn resolve_ip_binary_returns_absolute_path_or_none() {
        // The resolver must NEVER return a relative path. Either an
        // absolute path that exists, or None when the rootfs is
        // missing `ip` entirely. macOS dev hosts may legitimately
        // return None — `iproute2` is Linux-only.
        if let Some(path) = resolve_ip_binary() {
            assert!(path.starts_with('/'), "ip path must be absolute");
            assert!(
                std::path::Path::new(path).exists(),
                "resolved path {path} must exist"
            );
        }
    }

    #[test]
    fn agent_config_loads_full_yaml() {
        let yaml = r#"
agent:
  device_id: "test-device"
  name: "Test"
mavlink:
  port: "/dev/ttyACM0"
  baud: 57600
cloud:
  mqtt_broker: "broker.example"
  mqtt_port: 1883
  mqtt_use_tls: false
  convex_url: "https://relay.example"
  api_key: "secret"
api:
  bind: "127.0.0.1:9090"
"#;
        let parsed: AgentConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.agent.device_id, "test-device");
        assert_eq!(parsed.mavlink.port, "/dev/ttyACM0");
        assert_eq!(parsed.mavlink.baud, 57_600);
        assert_eq!(parsed.cloud.mqtt_broker, "broker.example");
        assert!(!parsed.cloud.mqtt_use_tls);
        assert_eq!(parsed.api.bind, "127.0.0.1:9090");
    }

    #[tokio::test]
    async fn persist_pair_code_writes_pairing_json() {
        let dir = tempfile::tempdir().unwrap();
        let agent_yaml = dir.path().join("agent.yaml");
        std::fs::write(
            &agent_yaml,
            r#"agent:
  device_id: "test"
  name: "Test"
cloud:
  mqtt_broker: ""
"#,
        )
        .unwrap();

        // Pair code goes to pairing.json (sibling of agent.yaml), NOT
        // to agent.yaml's cloud.api_key. The two values are different
        // per the proto: pair_code is a 6-char operator-typed code,
        // api_key is the longer ados_<base64url-32> the cloud relay
        // returns after a successful claim.
        persist_pair_code(&agent_yaml, "ABCD1234").await.unwrap();

        let pairing_path = dir.path().join("pairing.json");
        assert!(pairing_path.exists(), "pairing.json should be created");
        let store = ados_setup::pairing::PairingStore::new(&pairing_path);
        let state = store.load().unwrap();
        assert_eq!(state.pairing_code.as_deref(), Some("ABCD1234"));
        assert!(!state.is_paired(), "set_code clears the paired flag");
    }

    #[tokio::test]
    async fn persist_pair_code_uppercases_lowercase_input() {
        let dir = tempfile::tempdir().unwrap();
        let agent_yaml = dir.path().join("agent.yaml");
        std::fs::write(&agent_yaml, "agent:\n  device_id: \"x\"\n").unwrap();
        persist_pair_code(&agent_yaml, "abcd1234").await.unwrap();
        let pairing_path = dir.path().join("pairing.json");
        let state = ados_setup::pairing::PairingStore::new(&pairing_path)
            .load()
            .unwrap();
        // PairingStore::set_code uppercases — operator-typed lowercase
        // and uppercase resolve to the same canonical code.
        assert_eq!(state.pairing_code.as_deref(), Some("ABCD1234"));
    }

    #[tokio::test]
    async fn persist_pair_code_does_not_touch_agent_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let agent_yaml = dir.path().join("agent.yaml");
        let original = "agent:\n  device_id: \"test\"\n  name: \"Test\"\ncloud:\n  mqtt_broker: \"broker\"\n";
        std::fs::write(&agent_yaml, original).unwrap();
        persist_pair_code(&agent_yaml, "ABCD1234").await.unwrap();
        let after = std::fs::read_to_string(&agent_yaml).unwrap();
        assert_eq!(
            after, original,
            "agent.yaml must not be rewritten by pair (the code goes to pairing.json)"
        );
    }

    #[tokio::test]
    async fn persist_pair_code_rejects_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(&path, "{}").unwrap();
        let err = persist_pair_code(&path, "").await.unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn autogen_pair_code_creates_pairing_json_and_returns_code() {
        // Fresh temp dir, no pre-existing pairing.json. The first
        // invocation should mint a code, persist it, and the file
        // should appear with a 6-character code.
        let dir = tempfile::tempdir().unwrap();
        let agent_yaml = dir.path().join("agent.yaml");
        std::fs::write(&agent_yaml, "agent:\n  device_id: \"x\"\n").unwrap();

        autogen_pair_code(&agent_yaml).await.unwrap();

        let pairing_path = dir.path().join("pairing.json");
        assert!(
            pairing_path.exists(),
            "pairing.json should be created by --autogen"
        );
        let state = ados_setup::pairing::PairingStore::new(&pairing_path)
            .load()
            .unwrap();
        let code = state.pairing_code.expect("autogen produced no code");
        assert_eq!(code.len(), 6, "pair code is 6 chars");
        assert!(
            code.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "pair code uses safe charset only: {}",
            code
        );
    }

    #[tokio::test]
    async fn run_pair_routes_positional_to_persist() {
        let dir = tempfile::tempdir().unwrap();
        let agent_yaml = dir.path().join("agent.yaml");
        std::fs::write(&agent_yaml, "agent:\n  device_id: \"x\"\n").unwrap();

        run_pair(&agent_yaml, Some("ABC123".to_string()), false)
            .await
            .unwrap();

        let pairing_path = dir.path().join("pairing.json");
        let state = ados_setup::pairing::PairingStore::new(&pairing_path)
            .load()
            .unwrap();
        assert_eq!(state.pairing_code.as_deref(), Some("ABC123"));
    }

    #[tokio::test]
    async fn run_pair_routes_autogen_when_no_code() {
        let dir = tempfile::tempdir().unwrap();
        let agent_yaml = dir.path().join("agent.yaml");
        std::fs::write(&agent_yaml, "agent:\n  device_id: \"x\"\n").unwrap();

        run_pair(&agent_yaml, None, true).await.unwrap();

        let pairing_path = dir.path().join("pairing.json");
        let state = ados_setup::pairing::PairingStore::new(&pairing_path)
            .load()
            .unwrap();
        assert!(state.pairing_code.is_some(), "autogen should mint a code");
    }

    #[tokio::test]
    async fn run_pair_errors_when_neither_code_nor_autogen() {
        let dir = tempfile::tempdir().unwrap();
        let agent_yaml = dir.path().join("agent.yaml");
        std::fs::write(&agent_yaml, "agent:\n  device_id: \"x\"\n").unwrap();
        let err = run_pair(&agent_yaml, None, false).await.unwrap_err();
        // The error message guides the operator to the two valid
        // invocations; both forms must be present so the help is
        // self-contained.
        let msg = err.to_string();
        assert!(msg.contains("--autogen"), "error message: {}", msg);
    }

    #[test]
    fn tempfile_guard_unlinks_on_drop() {
        // Defense-in-depth: armed guard removes the underlying file on
        // Drop. Any early-exit path between fetch and exec relies on
        // this so a fetched script body cannot persist on disk after
        // a panic or `?`-bubbled error.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("install-script.sh");
        std::fs::write(&target, b"#!/bin/sh\necho hi\n").unwrap();
        assert!(target.exists());
        {
            let _guard = TempfileGuard::new(target.clone());
        }
        assert!(!target.exists(), "TempfileGuard did not remove file on Drop");
    }

    #[test]
    fn tempfile_guard_disarm_retains_file() {
        // The forensic-retention path on script-execution failure
        // disarms the guard so Drop becomes a no-op and the operator
        // can inspect the failing body.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("install-script.sh");
        std::fs::write(&target, b"#!/bin/sh\necho hi\n").unwrap();
        {
            let mut guard = TempfileGuard::new(target.clone());
            guard.disarm();
        }
        assert!(
            target.exists(),
            "disarmed TempfileGuard removed file on Drop"
        );
    }

    #[test]
    fn validate_config_rejects_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.yaml");
        assert!(!run_validate_config(&missing));
    }

    #[test]
    fn validate_config_rejects_bad_bind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        // /tmp exists on every Unix host so the mavlink.port check
        // passes; we want only the bad bind to fail validation.
        std::fs::write(
            &path,
            "agent:\n  device_id: \"x\"\nmavlink:\n  port: \"/tmp\"\napi:\n  bind: \"not a socket addr\"\n",
        )
        .unwrap();
        assert!(!run_validate_config(&path));
    }

    #[test]
    fn validate_config_rejects_missing_mavlink_port() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(
            &path,
            "agent:\n  device_id: \"x\"\nmavlink:\n  port: \"/dev/definitely-not-a-real-tty-zzz\"\napi:\n  bind: \"127.0.0.1:8080\"\n",
        )
        .unwrap();
        assert!(!run_validate_config(&path));
    }

    #[test]
    fn validate_config_rejects_bad_convex_url_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(
            &path,
            "agent:\n  device_id: \"x\"\nmavlink:\n  port: \"/tmp\"\ncloud:\n  convex_url: \"relay.example\"\napi:\n  bind: \"127.0.0.1:8080\"\n",
        )
        .unwrap();
        assert!(!run_validate_config(&path));
    }

    #[test]
    fn validate_config_passes_well_formed_file() {
        // /tmp is a directory, not a tty, but Path::exists() returns
        // true for it which is what the validator checks. Production
        // ttys live at paths like /dev/ttyACM0 which the agent itself
        // opens at runtime; the validator stays cheap.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(
            &path,
            "agent:\n  device_id: \"x\"\nmavlink:\n  port: \"/tmp\"\ncloud:\n  convex_url: \"https://relay.example\"\napi:\n  bind: \"127.0.0.1:8080\"\n",
        )
        .unwrap();
        assert!(run_validate_config(&path));
    }
}
