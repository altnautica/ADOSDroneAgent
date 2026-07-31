//! Path constants + architecture / OS probe.
//!
//! These are the canonical on-disk locations the installer reads and writes.
//! They mirror the bash installer's layout (`/opt/ados`, `/etc/ados`,
//! `/var/lib/ados`) so a Rust-driven install lands files exactly where the
//! running agent + the bash `--upgrade` path already expect them.

/// Agent install root — venv, bins, and persisted source live under here.
pub const INSTALL_DIR: &str = "/opt/ados";
/// Prebuilt service binaries (one file per Rust service).
pub const BIN_DIR: &str = "/opt/ados/bin";
/// Python virtualenv hosting the agent package + the ecosystem layers.
pub const VENV_DIR: &str = "/opt/ados/venv";
/// Portable, self-contained CPython runtime the venv step provisions when the
/// host carries no system Python 3.11+ (e.g. Debian 11, whose system Python is
/// 3.9). The relocatable build is extracted here; its interpreter is
/// `<dir>/bin/python3`.
pub const PORTABLE_PYTHON_DIR: &str = "/opt/ados/python";
/// Operator config + identity (config.yaml, profile.conf, pairing, device-id).
pub const CONFIG_DIR: &str = "/etc/ados";
/// Mutable agent state (install-result, checkpoints, peripherals).
pub const STATE_DIR: &str = "/var/lib/ados";

/// The persisted access-point passphrase. Read by the closing summary so the
/// operator learns a value that is now generated per unit rather than being
/// one published default across every box.
pub const AP_PASSPHRASE_PATH: &str = "/etc/ados/ap-passphrase";
/// Per-step `<name>.done` markers so an interrupted install resumes.
pub const CHECKPOINT_DIR: &str = "/var/lib/ados/install-checkpoints";
/// The machine-readable install outcome the heartbeat + GCS consume.
pub const RESULT_PATH: &str = "/var/lib/ados/install-result.json";
/// The top-level systemd unit the install starts and health-gates on.
pub const SERVICE_NAME: &str = "ados-supervisor";
/// Persisted source-tree copy the bash installer leaves behind so an
/// `--upgrade` invoked outside a fresh clone still finds the unit files,
/// udev rules, and driver scripts. The Rust installer's downstream steps fall
/// back to this (then `INSTALL_DIR/repo`) when `ctx.source_dir` is `None`.
pub const PERSISTED_SOURCE_DIR: &str = "/opt/ados/source";
/// The device-id file: a normalized 12-hex string (no dashes), never rewritten.
pub const DEVICE_ID_FILE: &str = "/etc/ados/device-id";
/// On-disk profile selector read by the agent + the bash `resolve_profile`.
pub const PROFILE_CONF: &str = "/etc/ados/profile.conf";
/// The operator config the agent reads on boot.
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";
/// Pairing material written by `--pair CODE`.
pub const PAIRING_JSON: &str = "/etc/ados/pairing.json";
/// Cloud-relay endpoint baked into the default config's `pairing.convex_url`.
pub const CONVEX_URL: &str = "https://convex-site.altnautica.com";

/// Resolve the source repo dir, mirroring the bash `SYSTEMD_SRC_DIR` /
/// driver-script resolution: prefer the path the clone recorded (`recorded`,
/// i.e. `ctx.source_dir`), then the persisted `/opt/ados/source`, then
/// `INSTALL_DIR/repo`. Returns the first that exists, or `None`.
pub fn resolve_source_dir(recorded: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let candidates: Vec<PathBuf> = [
        recorded.map(PathBuf::from),
        Some(PathBuf::from(PERSISTED_SOURCE_DIR)),
        Some(PathBuf::from(format!("{INSTALL_DIR}/repo"))),
    ]
    .into_iter()
    .flatten()
    .collect();
    candidates.into_iter().find(|p| p.is_dir())
}

/// The device's current hostname: `/etc/hostname`, then `uname -n`, else `ados`.
/// Used for the `<host>.local` setup URL and to pre-fill the name prompt.
pub fn read_hostname() -> String {
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let v = s.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    let res = crate::exec::run("uname", &["-n"]);
    if res.success() {
        let v = res.stdout.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    "ados".to_string()
}

/// The current hostname when it is a real, usable mDNS name (not empty,
/// `localhost`, a loopback literal, or the bare `ados` read-failure fallback),
/// for pre-filling the device-name prompt. `None` → use a synthesized default.
pub fn current_hostname() -> Option<String> {
    let h = read_hostname();
    let lower = h.to_ascii_lowercase();
    if h.is_empty()
        || lower == "ados"
        || lower == "localhost"
        || lower == "localhost.localdomain"
        || h.starts_with("127.")
    {
        None
    } else {
        Some(h)
    }
}

/// The device's routable LAN IPv4 addresses (from `ip -o -4 addr show`),
/// excluding loopback and the hotspot AP gateway (192.168.4.1). Used to show a
/// reach IP alongside the `.local` name — the IP works everywhere, including a
/// hosted GCS, while `.local` (mDNS) needs a desktop/localhost app.
pub fn lan_ips() -> Vec<String> {
    let res = crate::exec::run("ip", &["-o", "-4", "addr", "show"]);
    if !res.success() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for line in res.stdout.lines() {
        let mut it = line.split_whitespace();
        while let Some(tok) = it.next() {
            if tok == "inet" {
                if let Some(cidr) = it.next() {
                    let ip = cidr.split('/').next().unwrap_or(cidr).to_string();
                    if !ip.starts_with("127.") && ip != "192.168.4.1" && !out.contains(&ip) {
                        out.push(ip);
                    }
                }
                break;
            }
        }
    }
    out
}

/// The on-disk markers that, taken together, mean "the agent is already
/// installed on this box". Used by the install-presence probe so a bare
/// `--pair CODE` on an installed agent can run a fast re-pair instead of the
/// full fresh-install chain.
///
/// All three must be present: the venv interpreter (the agent package lives
/// there), the deployed supervisor unit (the install reached the systemd step),
/// and the persisted device identity (the install reached config/identity). A
/// partial install with only one or two of these is treated as NOT installed,
/// so a re-pair never runs against a half-provisioned box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallMarkers {
    /// `/opt/ados/venv/bin/python` exists.
    pub venv_python: bool,
    /// A deployed `ados-supervisor.service` unit file is on disk.
    pub supervisor_unit: bool,
    /// `/etc/ados/device-id` exists and is non-empty.
    pub device_id: bool,
}

/// Pure: are all the install markers present? Kept separate from the probe so a
/// unit test can exercise the all-or-nothing rule without touching disk.
pub fn install_present(markers: InstallMarkers) -> bool {
    markers.venv_python && markers.supervisor_unit && markers.device_id
}

/// The unit-file locations a deployed supervisor unit may live in (the
/// `/etc/systemd/system` drop-in the installer writes, or a packaged unit).
const SUPERVISOR_UNIT_DIRS: &[&str] = &["/etc/systemd/system", "/usr/lib/systemd/system"];

/// Probe the running box for an existing install. Reads only the on-disk
/// markers ([`InstallMarkers`]); does NOT contact systemd or the agent, so it is
/// cheap and safe to call before the run mode is resolved. Returns true only
/// when every marker is present (see [`install_present`]).
pub fn probe_install_present() -> bool {
    use std::path::Path;
    let venv_python = Path::new(&format!("{VENV_DIR}/bin/python")).exists();
    let supervisor_unit = SUPERVISOR_UNIT_DIRS
        .iter()
        .any(|dir| Path::new(dir).join("ados-supervisor.service").is_file());
    let device_id = std::fs::read_to_string(DEVICE_ID_FILE)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    install_present(InstallMarkers {
        venv_python,
        supervisor_unit,
        device_id,
    })
}

/// Resolved host facts the steps gate on. Kept tiny; richer HAL detection is a
/// later phase that runs the dedicated probe crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvInfo {
    /// Normalized architecture (`aarch64` for aarch64/arm64, else the raw arch).
    pub arch: String,
    /// `std::env::consts::OS` (`linux` on an SBC, `macos` on a dev host).
    pub os: String,
    /// Whether the host arch is one the prebuilt binaries target.
    pub supported_arch: bool,
}

impl EnvInfo {
    /// Probe the running host.
    pub fn probe() -> Self {
        EnvInfo {
            arch: arch().to_string(),
            os: std::env::consts::OS.to_string(),
            supported_arch: is_supported_arch(),
        }
    }
}

/// Normalized architecture string. The prebuilt assets are all `*-aarch64`, so
/// `arm64` (the macOS/Apple-silicon spelling) collapses to `aarch64`; anything
/// else passes through unchanged for reporting.
pub fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" | "arm64" => "aarch64",
        other => other,
    }
}

/// True when the running architecture is one the prebuilt binaries target.
/// The agent ships `*-aarch64` assets only.
pub fn is_supported_arch() -> bool {
    arch() == "aarch64"
}

/// Write `contents` to `path` atomically AND durably: a temp sibling, written,
/// flushed, `fsync`ed, then renamed over the destination.
///
/// The `fsync` is the load-bearing part, and the part the other ad-hoc copies of
/// this helper in the tree omit. Without it the rename can reach the disk before
/// the data blocks do, so a board losing power mid-write can still come back to
/// a zero-length file behind a rename that looked like it succeeded. First-run
/// install is precisely when a board is most likely to lose power — it is often
/// the first time the operator has it powered at all — and a truncated
/// `config.yaml` or `pairing.json` is not something a customer can recover from
/// in the field.
///
/// `mode` is applied to the temp file BEFORE the rename, so the file is never
/// briefly world-readable at its final path.
pub fn write_atomic_durable(
    path: &std::path::Path,
    contents: &[u8],
    mode: Option<u32>,
) -> std::io::Result<()> {
    use std::io::Write as _;

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "ados".to_string());
    let tmp = parent.join(format!("{file_name}.{}.tmp", std::process::id()));

    let write = (|| -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if let Some(m) = mode {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(m);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(contents)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write;
    }

    // Belt and braces: the open mode does not stick under every umask.
    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m));
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Extract the profile name from a `profile.conf` body (pure).
///
/// Accepts BOTH the `profile: <name>` line `config_identity::profile_conf_body`
/// writes and the legacy `profile=<name>` form the older bash installer wrote,
/// because the runtime parser in `ados-control` accepts both and this one must
/// not disagree with it. When it did, a device provisioned by the older
/// installer read its profile correctly at runtime but returned `None` here, so
/// an `--upgrade` invoked with no `--profile` fell through to the `drone`
/// default and tore down that device's ground-station units — the failure this
/// function exists to prevent, still reachable on every box installed before the
/// Rust installer landed.
///
/// Comments and blank lines are skipped and either quote style is tolerated, so
/// the two parsers agree on the whole file, not just the happy line.
pub fn parse_profile_conf(body: &str) -> Option<String> {
    for line in body.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let raw = if let Some(rest) = stripped.strip_prefix("profile:") {
            rest
        } else if let Some(rest) = stripped.strip_prefix("profile=") {
            rest
        } else {
            continue;
        };
        let v = raw.trim().trim_matches(|c| c == '"' || c == '\'');
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

/// Read the persisted install profile from `/etc/ados/profile.conf`, if present.
/// This is what lets an `--upgrade` (or `ados update`) invoked with no
/// `--profile` flag PRESERVE a non-drone box's profile instead of silently
/// re-provisioning it as the `drone` default and tearing down its profile units.
/// Returns `None` on a fresh box (no file), an unreadable file, or an empty
/// profile line — in which case the caller keeps its own default.
pub fn read_persisted_profile() -> Option<String> {
    let body = std::fs::read_to_string(PROFILE_CONF).ok()?;
    parse_profile_conf(&body)
}

/// Extract an arbitrary `key: value` / `key=value` line from a conf body,
/// with the same tolerance [`parse_profile_conf`] applies.
pub fn parse_conf_value(body: &str, key: &str) -> Option<String> {
    let yaml = format!("{key}:");
    let kv = format!("{key}=");
    for line in body.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let raw = if let Some(rest) = stripped.strip_prefix(&yaml) {
            rest
        } else if let Some(rest) = stripped.strip_prefix(&kv) {
            rest
        } else {
            continue;
        };
        let v = raw.trim().trim_matches(|c| c == '"' || c == '\'');
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

/// Read the persisted release channel from `/etc/ados/profile.conf`.
///
/// The same preservation problem as the profile, with a sharper consequence. An
/// upgrade invoked with no `--channel` used to fall back to the compiled-in
/// `edge` default, so a device deliberately installed on `stable` silently
/// defected to tip-of-main on its first update — and took its signature
/// enforcement with it, since verification is channel-gated. Updating is one
/// keystroke from the status screen, so that happened without anyone choosing
/// it.
///
/// `None` on a fresh box or a conf with no channel line, in which case the
/// caller keeps its own default.
pub fn read_persisted_channel() -> Option<String> {
    let body = std::fs::read_to_string(PROFILE_CONF).ok()?;
    parse_conf_value(&body, "channel")
}

/// Read the persisted pinned version from `/etc/ados/profile.conf`. Only
/// meaningful on the `stable` channel, which installs an explicit release.
pub fn read_persisted_version() -> Option<String> {
    let body = std::fs::read_to_string(PROFILE_CONF).ok()?;
    parse_conf_value(&body, "version")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_profile_conf_reads_the_profile_line() {
        assert_eq!(
            parse_profile_conf("profile: ground_station\n").as_deref(),
            Some("ground_station")
        );
        // A quoted value (config.yaml style) is unwrapped.
        assert_eq!(
            parse_profile_conf("profile: \"drone\"\n").as_deref(),
            Some("drone")
        );
        // Surrounding whitespace is tolerated.
        assert_eq!(
            parse_profile_conf("  profile:   workstation  \n").as_deref(),
            Some("workstation")
        );
        // The profile line is found among other keys.
        assert_eq!(
            parse_profile_conf("device: abc\nprofile: compute\n").as_deref(),
            Some("compute")
        );
    }

    #[test]
    fn write_atomic_durable_lands_content_mode_and_no_residue() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ados-installer-env-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let path = dir.join("nested").join("config.yaml");

        write_atomic_durable(&path, b"first: value\n", Some(0o600)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first: value\n");

        // An overwrite is complete, not appended or partial.
        write_atomic_durable(&path, b"second\n", Some(0o600)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a credential file must not be world-readable");
        }

        // No temp sibling is left behind for either write.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must not survive");
    }

    #[test]
    fn a_persisted_channel_survives_an_upgrade_with_no_flag() {
        // The install identity an upgrade must preserve. Losing the channel is
        // what silently moved a stable device to tip-of-main and dropped its
        // signature enforcement, which is channel-gated.
        let body = "profile: ground_station\nchannel: stable\nversion: 1.2.3\n";
        assert_eq!(parse_conf_value(body, "channel").as_deref(), Some("stable"));
        assert_eq!(parse_conf_value(body, "version").as_deref(), Some("1.2.3"));
        assert_eq!(
            parse_conf_value(body, "profile").as_deref(),
            Some("ground_station")
        );

        // An older conf carries no channel, so the caller keeps its default
        // rather than inventing one.
        assert_eq!(parse_conf_value("profile: drone\n", "channel"), None);

        // Same tolerance as the profile line: legacy form, quotes, comments.
        assert_eq!(
            parse_conf_value("channel=stable\n", "channel").as_deref(),
            Some("stable")
        );
        assert_eq!(
            parse_conf_value("channel: \"edge\"\n", "channel").as_deref(),
            Some("edge")
        );
        assert_eq!(parse_conf_value("# channel: stable\n", "channel"), None);

        // A key must not match a longer key that starts with it.
        assert_eq!(parse_conf_value("channel_extra: x\n", "channel"), None);
    }

    #[test]
    fn parse_profile_conf_reads_the_legacy_key_value_form() {
        // The older bash installer wrote `profile=X`. The runtime parser in
        // `ados-control` accepts it; this one must too. When it did not, a
        // ground station provisioned by that installer read `None` here, so an
        // upgrade with no explicit profile fell through to the drone default
        // and tore its own units down.
        assert_eq!(
            parse_profile_conf("profile=ground_station\n").as_deref(),
            Some("ground_station")
        );
        assert_eq!(
            parse_profile_conf("profile='ground-station'\n").as_deref(),
            Some("ground-station"),
            "the legacy form also appeared single-quoted"
        );
        assert_eq!(
            parse_profile_conf("  profile=  workstation  \n").as_deref(),
            Some("workstation")
        );
        // A commented-out line is not a value, in either form.
        assert_eq!(parse_profile_conf("# profile=drone\n"), None);
        assert_eq!(parse_profile_conf("# profile: drone\n"), None);
        // A real value below a comment is still found.
        assert_eq!(
            parse_profile_conf("# written by the installer\nprofile=compute\n").as_deref(),
            Some("compute")
        );
    }

    #[test]
    fn parse_profile_conf_none_when_absent_or_empty() {
        assert_eq!(parse_profile_conf(""), None);
        assert_eq!(parse_profile_conf("profile=\n"), None);
        assert_eq!(parse_profile_conf("profile:\n"), None);
        assert_eq!(parse_profile_conf("profile: \"\"\n"), None);
        assert_eq!(parse_profile_conf("other: value\n"), None);
    }

    #[test]
    fn arch_normalizes_arm64_to_aarch64() {
        // On any host the helper must return one of the canonical spellings;
        // on a real aarch64/arm64 host that is exactly "aarch64".
        let a = arch();
        assert!(!a.is_empty());
        if std::env::consts::ARCH == "arm64" || std::env::consts::ARCH == "aarch64" {
            assert_eq!(a, "aarch64");
            assert!(is_supported_arch());
        }
    }

    #[test]
    fn paths_are_under_the_canonical_roots() {
        assert!(BIN_DIR.starts_with(INSTALL_DIR));
        assert!(VENV_DIR.starts_with(INSTALL_DIR));
        assert!(CHECKPOINT_DIR.starts_with(STATE_DIR));
        assert!(RESULT_PATH.starts_with(STATE_DIR));
    }

    #[test]
    fn probe_is_self_consistent() {
        let e = EnvInfo::probe();
        assert_eq!(e.arch, arch());
        assert_eq!(e.supported_arch, is_supported_arch());
        assert_eq!(e.os, std::env::consts::OS);
    }

    #[test]
    fn install_present_requires_all_markers() {
        let all = InstallMarkers {
            venv_python: true,
            supervisor_unit: true,
            device_id: true,
        };
        assert!(install_present(all));

        // Any single marker missing → not installed (a partial install must
        // never be treated as ready for a fast re-pair).
        assert!(!install_present(InstallMarkers {
            venv_python: false,
            ..all
        }));
        assert!(!install_present(InstallMarkers {
            supervisor_unit: false,
            ..all
        }));
        assert!(!install_present(InstallMarkers {
            device_id: false,
            ..all
        }));
        // Nothing present → not installed.
        assert!(!install_present(InstallMarkers {
            venv_python: false,
            supervisor_unit: false,
            device_id: false,
        }));
    }

    #[test]
    fn resolve_source_dir_prefers_recorded_then_falls_back() {
        // A recorded path that exists wins.
        let dir = tempfile::tempdir().unwrap();
        let got = resolve_source_dir(Some(dir.path()));
        assert_eq!(got.as_deref(), Some(dir.path()));

        // A recorded path that does NOT exist falls through to the canonical
        // fallbacks (neither of which exists on a dev host) → None.
        let missing = dir.path().join("nope");
        // On a real SBC `/opt/ados/source` may exist; this assertion only holds
        // on a host where neither fallback dir is present (the CI/dev case).
        if !std::path::Path::new(PERSISTED_SOURCE_DIR).is_dir()
            && !std::path::Path::new(&format!("{INSTALL_DIR}/repo")).is_dir()
        {
            assert_eq!(resolve_source_dir(Some(&missing)), None);
            assert_eq!(resolve_source_dir(None), None);
        }
    }
}
