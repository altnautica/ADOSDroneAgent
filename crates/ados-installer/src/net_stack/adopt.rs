//! Carry out an [`AdoptionPlan`]: hand the management Wi-Fi to NetworkManager
//! and leave every wired link with the stack that runs it.

use std::path::Path;
use std::time::{Duration, Instant};

use ados_term::exec;
use ados_term::progress::ProgressSink;

use super::{plan, probe, AdoptionPlan, NETPLAN_NM_PATH};
use crate::steps::deps::{apt_update, apt_waiting_for_lock};
use crate::steps::reboot;

/// The handoff drop-in's file name. Its own `[device-…]` section, so it holds
/// whatever order NetworkManager merges `/run` and `/etc` in (it reads each
/// directory in turn, so a `/run` `unmanaged-devices` list is replaced by the
/// WFB rule's in `/etc`).
const HANDOFF_CONF: &str = "96-ados-wired-handoff.conf";

/// systemd-networkd drop-in that releases the Wi-Fi interface.
const NETWORKD_WIFI_UNMANAGED: &str = "/etc/systemd/network/00-ados-wifi-unmanaged.network";

/// The reason recorded in the pending-reboot signal for the netplan switch.
pub const REBOOT_REASON: &str = "network-stack netplan renderer NetworkManager";

/// How long NetworkManager gets to pick the Wi-Fi interface up after starting.
const NM_TAKEOVER: Duration = Duration::from_secs(20);

/// The outcome of a successful adoption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adopted {
    /// The stack the host ran before (see [`super::NetStack::label`]).
    pub stack: &'static str,
    /// Set when the switch completes at the next boot.
    pub reboot_reason: Option<String>,
}

/// Probe the host, plan the adoption and carry it out. A no-op plan touches
/// nothing. On error, everything already done stays done: each change is safe
/// on its own and a re-run picks up from what the probe then finds.
pub fn ensure_networkmanager(sink: &ProgressSink) -> Result<Adopted, String> {
    let stack = probe();
    let label = stack.label();
    let p = plan(&stack);
    if p.noop() {
        let reboot_reason = p.netplan_reboot_pending.then(|| {
            reboot::signal_required(REBOOT_REASON);
            REBOOT_REASON.to_string()
        });
        return Ok(Adopted {
            stack: label,
            reboot_reason,
        });
    }
    // Taking the Wi-Fi from its current owner would cut the link this host
    // (or the operator's session) is using, with nothing to bring it back.
    if wifi_uplink_in_use() {
        return Err("this device is online over Wi-Fi; its network stack is left as it is".into());
    }
    let switched = execute(&p, sink)?;
    Ok(Adopted {
        stack: label,
        reboot_reason: (switched || p.netplan_reboot_pending).then(|| REBOOT_REASON.to_string()),
    })
}

/// True when the default route or the operator's SSH session goes out over
/// Wi-Fi.
pub fn wifi_uplink_in_use() -> bool {
    crate::wizard::wifi::currently_on_wifi() || crate::wizard::wifi::session_rides_wifi()
}

/// Carry the plan out. Returns whether the netplan renderer switch was staged.
fn execute(p: &AdoptionPlan, sink: &ProgressSink) -> Result<bool, String> {
    // 0. A renderer switch netplan would reject at boot would leave the wired
    //    link unconfigured after the reboot: keep the handoff permanent instead.
    let switch = p.netplan_switch && netplan_accepts_networkmanager();
    if p.netplan_switch && !switch {
        tracing::warn!("netplan rejects the NetworkManager renderer for this config; wired stays with networkd");
    }

    // 1. The WFB radios are unmanaged before NetworkManager ever starts.
    crate::steps::wfb_unmanaged::write_rule()?;

    // 2. Wired links stay with their current owner (until the reboot that
    //    completes the switch, or for good without one).
    if !p.handoff_ifaces.is_empty() {
        let dir = if p.handoff_ephemeral && switch {
            "/run/NetworkManager/conf.d"
        } else {
            "/etc/NetworkManager/conf.d"
        };
        let path = Path::new(dir).join(HANDOFF_CONF);
        write_file(&path, &render_handoff_conf(&p.handoff_ifaces), 0o644)?;
    }

    // 3. Install NetworkManager (one index refresh on a first failure) while
    //    the old stack still runs the Wi-Fi, so a failed install changes
    //    nothing that was working.
    if p.install_nm {
        let argv = [
            "install",
            "-y",
            "--no-install-recommends",
            "network-manager",
            "wpasupplicant",
        ];
        let mut res = apt_waiting_for_lock(&argv, sink);
        if !res.success() && res.spawned {
            let _ = apt_update(sink);
            res = apt_waiting_for_lock(&argv, sink);
        }
        if !res.success() {
            return Err(format!(
                "could not install NetworkManager: {}",
                tail(&res.stderr)
            ));
        }
    }

    // 4. Release the Wi-Fi interface from its old owner.
    release_wifi(p)?;

    // 5. Start NetworkManager and wait for it to take the Wi-Fi interface.
    if p.start_nm || p.install_nm {
        let res = exec::run("systemctl", &["enable", "--now", "NetworkManager"]);
        if !res.success() {
            return Err(format!(
                "could not start NetworkManager: {}",
                tail(&res.stderr)
            ));
        }
    }
    if let Some(w) = &p.wifi_iface {
        wait_for_takeover(w)?;
    }

    // 6. netplan: switch the renderer for the next boot only.
    if switch {
        // Any systemd reload from here on re-runs the netplan generator, which
        // removes networkd's wired config at once; keep a copy it leaves alone,
        // so a networkd reload before the reboot keeps the wired address.
        keep_networkd_config()?;
        write_file(Path::new(NETPLAN_NM_PATH), NETPLAN_NM_BODY, 0o600)?;
        if p.disable_networkd_wait_online
            && exec::run("systemctl", &["is-enabled", "systemd-networkd-wait-online"])
                .stdout
                .trim()
                == "enabled"
        {
            let _ = exec::run(
                "systemctl",
                &["disable", "systemd-networkd-wait-online.service"],
            );
        }
    }
    if switch || p.netplan_reboot_pending {
        reboot::signal_required(REBOOT_REASON);
    }
    Ok(switch)
}

/// Copy netplan's generated networkd units in `/run/systemd/network` to
/// `09-ados-keep-*`, a name the generator never cleans. `/run` is cleared at the
/// reboot that completes the switch.
fn keep_networkd_config() -> Result<(), String> {
    let dir = Path::new("/run/systemd/network");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(rest) = name.strip_prefix("10-netplan-") {
            let body = std::fs::read_to_string(e.path())
                .map_err(|err| format!("could not read {name}: {err}"))?;
            write_file(&dir.join(format!("09-ados-keep-{rest}")), &body, 0o644)?;
        }
    }
    Ok(())
}

/// The renderer switch drop-in.
const NETPLAN_NM_BODY: &str =
    "# ADOS: NetworkManager renders every interface from the next boot.\n\
                               network:\n  version: 2\n  renderer: NetworkManager\n";

/// Whether netplan generates a configuration from this host's YAML with the
/// renderer switched, checked in a scratch root so nothing live is touched.
fn netplan_accepts_networkmanager() -> bool {
    let root = Path::new("/run/ados/netplan-check");
    let _ = std::fs::remove_dir_all(root);
    let etc = root.join("etc/netplan");
    let staged = std::fs::create_dir_all(&etc).is_ok()
        && std::fs::read_dir("/etc/netplan")
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
                    .all(|p| std::fs::copy(&p, etc.join(p.file_name().unwrap_or_default())).is_ok())
            })
            .unwrap_or(false)
        && write_file(
            &etc.join("90-ados-networkmanager.yaml"),
            NETPLAN_NM_BODY,
            0o600,
        )
        .is_ok();
    let ok = staged
        && exec::run(
            "netplan",
            &["generate", "--root-dir", &root.to_string_lossy()],
        )
        .success();
    let _ = std::fs::remove_dir_all(root);
    ok
}

/// Take the Wi-Fi interface away from systemd-networkd, ifupdown, dhcpcd or
/// its own `wpa_supplicant@` unit, whichever runs it.
fn release_wifi(p: &AdoptionPlan) -> Result<(), String> {
    if let Some(w) = &p.networkd_release_wifi {
        let body = format!("[Match]\nName={w}\n\n[Link]\nUnmanaged=yes\n");
        write_file(Path::new(NETWORKD_WIFI_UNMANAGED), &body, 0o644)?;
        let _ = exec::run("networkctl", &["reload"]);
    }
    if let Some(w) = &p.ifupdown_release_wifi {
        let _ = exec::run("ifdown", &[w]);
        for file in super::ifupdown_files() {
            let Ok(body) = std::fs::read_to_string(&file) else {
                continue;
            };
            if let Some(new) = comment_out_iface(&body, w) {
                let bak = file.with_file_name(format!(
                    "{}.ados-bak",
                    file.file_name().unwrap_or_default().to_string_lossy()
                ));
                if !bak.exists() {
                    std::fs::copy(&file, &bak)
                        .map_err(|e| format!("could not back up {}: {e}", file.display()))?;
                }
                std::fs::write(&file, new)
                    .map_err(|e| format!("could not edit {}: {e}", file.display()))?;
            }
        }
    }
    if let Some(w) = &p.dhcpcd_deny_wifi {
        let conf = Path::new("/etc/dhcpcd.conf");
        let body = std::fs::read_to_string(conf).unwrap_or_default();
        if let Some(new) = dhcpcd_deny(&body, w) {
            std::fs::write(conf, new)
                .map_err(|e| format!("could not edit {}: {e}", conf.display()))?;
        }
        let _ = exec::run("dhcpcd", &["-k", w]);
    }
    if let Some(w) = &p.disable_wpa_unit {
        let unit = format!("wpa_supplicant@{w}");
        let _ = exec::run("systemctl", &["disable", "--now", &unit]);
    }
    Ok(())
}

/// Poll NetworkManager until it manages `iface`.
fn wait_for_takeover(iface: &str) -> Result<(), String> {
    let deadline = Instant::now() + NM_TAKEOVER;
    let mut radio_on = false;
    loop {
        let res = exec::run(
            "nmcli",
            &["-t", "-f", "DEVICE,TYPE,STATE", "device", "status"],
        );
        let state = device_state(&res.stdout, iface);
        match state.as_deref() {
            Some(s) if s != "unmanaged" && s != "unavailable" => return Ok(()),
            Some("unavailable") if !radio_on => {
                // A radio NetworkManager last saw switched off stays off.
                radio_on = true;
                let _ = exec::run("nmcli", &["radio", "wifi", "on"]);
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(format!("NetworkManager did not take over {iface}"));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The state column for `iface` in `nmcli -t -f DEVICE,TYPE,STATE device
/// status` output (pure).
pub(super) fn device_state(out: &str, iface: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let cols = crate::wizard::wifi::split_terse(l);
        (cols.len() >= 3 && cols[0] == iface).then(|| cols[2].trim().to_string())
    })
}

/// The handoff drop-in body: NetworkManager leaves these wired interfaces to
/// the stack already running them (pure).
pub fn render_handoff_conf(ifaces: &[String]) -> String {
    let list = ifaces
        .iter()
        .map(|i| format!("interface-name:{i}"))
        .collect::<Vec<_>>()
        .join(";");
    format!(
        "# ADOS: these wired links stay with the network stack that runs them.\n\
         # NetworkManager manages the Wi-Fi only. Written by the installer.\n\
         [device-ados-wired-handoff]\n\
         match-device={list}\n\
         managed=0\n"
    )
}

const IFUPDOWN_MARK: &str = "#ados-nm# ";

/// Comment every `auto`/`allow-*` mention and `iface` stanza of `iface` out
/// of an ifupdown file, or `None` when the file does not declare it (pure).
/// An `auto` line naming other interfaces too keeps them on a fresh line.
pub(super) fn comment_out_iface(body: &str, iface: &str) -> Option<String> {
    let mut out = String::with_capacity(body.len() + 64);
    let mut in_stanza = false;
    let mut changed = false;
    for line in body.lines() {
        let starts_block = ["auto", "allow-", "iface", "mapping", "source"]
            .iter()
            .any(|k| line.starts_with(k));
        if starts_block {
            in_stanza = false;
            let toks: Vec<&str> = line.split_whitespace().collect();
            let kw = toks.first().copied().unwrap_or("");
            if kw == "iface" && toks.get(1) == Some(&iface) {
                in_stanza = true;
            } else if (kw == "auto" || kw.starts_with("allow-")) && toks[1..].contains(&iface) {
                changed = true;
                out.push_str(IFUPDOWN_MARK);
                out.push_str(line);
                out.push('\n');
                let rest: Vec<&str> = toks[1..].iter().copied().filter(|t| *t != iface).collect();
                if !rest.is_empty() {
                    out.push_str(&format!("{kw} {}\n", rest.join(" ")));
                }
                continue;
            }
        }
        if in_stanza {
            changed = true;
            out.push_str(IFUPDOWN_MARK);
        }
        out.push_str(line);
        out.push('\n');
    }
    changed.then_some(out)
}

/// `body` with a `denyinterfaces <iface>` line appended, or `None` when dhcpcd
/// already denies it (pure).
pub(super) fn dhcpcd_deny(body: &str, iface: &str) -> Option<String> {
    let denied = body.lines().any(|l| {
        let mut t = l.split_whitespace();
        t.next() == Some("denyinterfaces") && t.any(|x| x == iface)
    });
    if denied {
        return None;
    }
    let mut out = body.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "\n# ADOS: Wi-Fi is managed by NetworkManager.\ndenyinterfaces {iface}\n"
    ));
    Some(out)
}

fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.trim().lines().collect();
    lines[lines.len().saturating_sub(3)..].join(" ")
}

/// Write `body` to `path` (parent created) with `mode`, atomically.
fn write_file(path: &Path, body: &str, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let err = |e: std::io::Error| format!("could not write {}: {e}", path.display());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(err)?;
    }
    let tmp = path.with_extension("ados-tmp");
    std::fs::write(&tmp, body).map_err(err)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(err)?;
    std::fs::rename(&tmp, path).map_err(err)
}
