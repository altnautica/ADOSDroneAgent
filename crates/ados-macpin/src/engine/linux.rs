//! Linux-only enumeration, link writing, udev reload and reconcile.

use super::*;
use crate::{AdapterState, AdapterVerdict};
use std::path::PathBuf;
use std::process::Command;

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Enumerate USB-and-other network adapters under `/sys/class/net`. Skips
/// loopback and interfaces with no backing device.
pub fn enumerate_net_adapters() -> Vec<NetAdapter> {
    let mut out = Vec::new();
    let read = match std::fs::read_dir("/sys/class/net") {
        Ok(r) => r,
        Err(_) => return out,
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "lo" {
            continue;
        }
        let base = entry.path();
        let device = base.join("device");
        // No device link -> virtual interface; not a pin target.
        if !device.exists() {
            continue;
        }
        let (usb_id, usb_path) = resolve_usb_identity(&device);
        let mac = std::fs::read_to_string(base.join("address"))
            .ok()
            .and_then(|s| MacAddr::parse(s.trim()));
        out.push(NetAdapter {
            name,
            usb_id,
            usb_path,
            mac,
        });
    }
    out
}

/// Walk up from the interface's `device` link to the USB device node that
/// carries `idVendor`/`idProduct`. Returns the id and the topology path
/// (the device node's directory basename, e.g. `5-1.3`).
fn resolve_usb_identity(device_link: &Path) -> (Option<UsbId>, String) {
    let mut cur = match std::fs::canonicalize(device_link) {
        Ok(p) => p,
        Err(_) => return (None, String::new()),
    };
    for _ in 0..6 {
        let vid = std::fs::read_to_string(cur.join("idVendor")).ok();
        let pid = std::fs::read_to_string(cur.join("idProduct")).ok();
        if let (Some(v), Some(p)) = (vid, pid) {
            let vid = u16::from_str_radix(v.trim(), 16).ok();
            let pid = u16::from_str_radix(p.trim(), 16).ok();
            let path = cur
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if let (Some(vid), Some(pid)) = (vid, pid) {
                return (Some(UsbId { vid, pid }), path);
            }
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }
    (None, String::new())
}

/// True when the systemd-udev `.link` mechanism is available to apply a
/// `[Link] MACAddress=` drop-in at device setup. The `net_setup_link` udev
/// builtin owns `.link` application and runs on ANY systemd host -- whether
/// the L3 manager is systemd-networkd OR NetworkManager (both run
/// systemd-udevd). So the pin is honored on NetworkManager-only boards too;
/// gating on networkd *running* (`/run/systemd/netif`) alone would wrongly
/// defer the pin on an NM board and let its MAC keep churning. We check the
/// udev runtime (`/run/udev`), with the networkd runtime dir as a fallback.
pub fn link_mechanism_available() -> bool {
    Path::new("/run/udev").exists() || Path::new("/run/systemd/netif").exists()
}

/// The adapter's udev `ID_PATH` (its stable USB-port topology id), or
/// `None` when `udevadm` is unavailable or the property is absent.
fn udev_id_path(iface: &str) -> Option<String> {
    let target = format!("/sys/class/net/{iface}");
    let out = Command::new("udevadm")
        .args(["info", "--query=property", "--path", &target])
        .output()
        .ok()?;
    parse_udev_id_path(&String::from_utf8_lossy(&out.stdout))
}

/// Resolve the `[Match]` block for `iface`: the adapter's stable USB-port
/// path (`Path=`), or `None` when udev does not know it. There is no
/// interface-name fallback: `OriginalName=` names whichever adapter
/// enumerated under that name this boot, so a pin keyed on it moves
/// between adapters when they swap order.
///
/// It deliberately does NOT mirror whichever stock `.link` wins for the
/// interface. That file is normally `99-default.link`, whose match is
/// `OriginalName=*`, and copying it into a drop-in carrying an
/// unconditional `MACAddress=` pinned ONE adapter's address onto every
/// interface on the box. [`match_block_is_specific`] is the write-time
/// backstop.
pub fn resolve_match_block(iface: &str) -> Option<String> {
    udev_id_path(iface).map(|p| format!("Path={p}"))
}

/// Write the pin `.link` for the adapter `key` into
/// `layout.networkd_dir`. Idempotent: a no-op when the file already has
/// identical content. Reloads udev so a later boot applies it; never
/// touches the live interface. Returns the file path.
///
/// Refused when `match_block` is not specific to one adapter: the file
/// carries an unconditional `MACAddress=`, so a glob match would give every
/// interface on the box the same address.
pub fn write_pin_link(
    layout: &PinLayout,
    key: &str,
    match_block: &str,
    mac: &MacAddr,
) -> std::io::Result<PathBuf> {
    if !match_block_is_specific(match_block) {
        return Err(std::io::Error::other(format!(
            "refusing to pin {key}: [Match] block {match_block:?} does not name one adapter"
        )));
    }
    let path = layout.networkd_dir.join(link_file_name(layout, key));
    let body = render_link_file(layout, match_block, mac);
    let unchanged = std::fs::read_to_string(&path)
        .map(|cur| cur == body)
        .unwrap_or(false);
    if !unchanged {
        write_atomic(&path, body.as_bytes(), 0o644)?;
        reload_udev();
    }
    Ok(path)
}

/// Remove the pin `.link` for the adapter `key` from
/// `layout.networkd_dir`. Returns whether a file was removed.
///
/// Async for request handlers: the udev reload is a subprocess whose
/// settle would otherwise block the reactor and stall every other
/// in-flight request on a single-core board.
pub async fn remove_pin_link(layout: &PinLayout, key: &str) -> std::io::Result<bool> {
    let path = layout.networkd_dir.join(link_file_name(layout, key));
    if path.exists() {
        std::fs::remove_file(&path)?;
        reload_udev_async().await;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Remove the stale pin `.link` files (see [`stale_pin_links`]). Returns
/// how many were removed.
fn sweep_pin_links(
    layout: &PinLayout,
    present: &std::collections::HashSet<String>,
    pinned: &std::collections::HashSet<String>,
) -> usize {
    let dir = layout.networkd_dir.as_path();
    let existing: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let mut removed = 0;
    for name in stale_pin_links(layout, &existing, present, pinned) {
        match std::fs::remove_file(dir.join(&name)) {
            Ok(()) => {
                removed += 1;
                tracing::info!(file = %name, "removed a stale MAC pin link");
            }
            Err(e) => {
                tracing::warn!(file = %name, error = %e, "stale MAC pin link not removed")
            }
        }
    }
    if removed > 0 {
        reload_udev();
    }
    removed
}

/// Re-tag the LIVE interface now (drops any connection over it). Opt-in; the
/// caller is responsible for the safety gate (never the management iface).
///
/// Async + bounded: it is meant for a request handler, and a blocking
/// `ip link set` on the reactor stalls every other request for the duration
/// of the link down/up. Each invocation is capped at 10 s with
/// `kill_on_drop`, so a wedged netlink call cannot park the handler forever.
pub async fn apply_live(iface: &str, mac: &MacAddr) -> std::io::Result<()> {
    let mac = mac.to_string();
    run_ip(&["link", "set", "dev", iface, "down"]).await?;
    run_ip(&["link", "set", "dev", iface, "address", &mac]).await?;
    run_ip(&["link", "set", "dev", iface, "up"]).await?;
    Ok(())
}

async fn run_ip(args: &[&str]) -> std::io::Result<()> {
    let status = tokio::time::timeout(
        IP_CMD_TIMEOUT,
        tokio::process::Command::new("ip")
            .args(args)
            .kill_on_drop(true)
            .status(),
    )
    .await
    .map_err(|_| std::io::Error::other(format!("ip {args:?} timed out")))??;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("ip {args:?} failed")))
    }
}

/// Reload the udev rules database so a later device event (a hot-plug, or
/// the next boot) picks up the new/removed `.link` file. Runs only the
/// invocations in [`super::UDEV_RELOAD_ARGV`], which deliberately excludes
/// any `udevadm trigger` of the `net` subsystem (see that const's docs).
///
/// Blocking form, for the synchronous [`reconcile`] path only, which runs
/// off the reactor. The async path uses [`reload_udev_async`].
fn reload_udev() {
    for argv in UDEV_RELOAD_ARGV {
        let _ = Command::new("udevadm").args(*argv).status();
    }
}

/// [`reload_udev`] for an async caller: bounded and `kill_on_drop`, so a
/// udev settle can neither block the reactor nor outlive the request.
async fn reload_udev_async() {
    for argv in UDEV_RELOAD_ARGV {
        let _ = tokio::time::timeout(
            UDEV_CMD_TIMEOUT,
            tokio::process::Command::new("udevadm")
                .args(*argv)
                .kill_on_drop(true)
                .status(),
        )
        .await;
    }
}

/// The full reconcile: enumerate -> fold the learner -> classify -> write
/// `.link`s for armed quirk/override randomizers -> persist + return state.
/// `with_learner` enables cross-boot detection of unknown randomizers (a
/// background service runs it; a one-shot provisioning step may leave it
/// off to keep the first boot deterministic). Files live under
/// `config.layout`.
pub fn reconcile(config: &ReconcileConfig, with_learner: bool) -> MacPinsState {
    let layout = &config.layout;
    let mut state = load_state_from(&layout.state_path);
    let adapters = enumerate_net_adapters();
    let machine_id = read_machine_id();
    let link_mechanism = link_mechanism_available();
    let now = now_unix();

    let mut verdicts = Vec::new();
    let mut present = std::collections::HashSet::new();
    let mut pinned = std::collections::HashSet::new();
    for a in &adapters {
        let usb_id = match a.usb_id {
            Some(id) => id,
            None => continue, // skip non-USB
        };
        let vidpid = format!("{:04x}:{:04x}", usb_id.vid, usb_id.pid);
        let key = adapter_key(&vidpid, &a.usb_path);
        present.insert(link_file_name(layout, &key));
        // Always salt the derived MAC by the adapter's stable USB path, so a
        // lone randomizer can never derive the same machine-id-only MAC as
        // another radio on the same box.
        let salt = a.usb_path.as_str();

        // Fold this boot into the learner memory for unknown adapters (not
        // quirk, not known-efuse, not override). The record is keyed by the
        // stable identity so a churning MAC + name still resolves to it.
        let is_unknown = !config.overrides.contains_key(&vidpid)
            && !config.overrides.contains_key(&key)
            && !is_known_stable_efuse(usb_id)
            && is_quirk_randomizer(usb_id).is_none();
        if is_unknown {
            if let Some(mac) = a.mac {
                fold_learner(&mut state.learner, &vidpid, &a.usb_path, mac, now);
            }
        }
        let learner_rec = state
            .learner
            .iter()
            .find(|r| r.vidpid == vidpid && r.usb_path == a.usb_path)
            .cloned();

        let decision = classify_adapter(
            usb_id,
            &key,
            &vidpid,
            machine_id.as_deref(),
            salt,
            config,
            link_mechanism,
            learner_rec.as_ref(),
            with_learner,
        );
        let verdict = realize(layout, decision, a, &vidpid, &key);
        if verdict.state == AdapterState::Pinned {
            pinned.insert(link_file_name(layout, &key));
        }
        verdicts.push(verdict);
    }
    sweep_pin_links(layout, &present, &pinned);
    state.adapters = verdicts;
    state.updated_at = now;
    let _ = save_state_to(&layout.state_path, &state);
    state
}

/// Turn a [`Decision`] into a verdict, writing the `.link` for a `Pin`.
fn realize(
    layout: &PinLayout,
    decision: Decision,
    a: &NetAdapter,
    vidpid: &str,
    key: &str,
) -> AdapterVerdict {
    let mut v = AdapterVerdict {
        name: a.name.clone(),
        vidpid: vidpid.to_string(),
        usb_path: a.usb_path.clone(),
        state: AdapterState::Stable,
        source: None,
        pinned_mac: None,
        last_seen_mac: a.mac,
        applied_live: false,
        link_file: None,
        deferred_reason: None,
    };
    match decision {
        Decision::Stable | Decision::Observe => {}
        Decision::Disabled => v.state = AdapterState::Disabled,
        Decision::Deferred(reason) => {
            v.state = AdapterState::Deferred;
            v.deferred_reason = Some(reason);
        }
        Decision::Candidate { proposed } => {
            v.state = AdapterState::Candidate;
            v.source = Some(AdapterSource::Learned);
            v.pinned_mac = proposed;
        }
        Decision::Pin { mac, source } => {
            let Some(match_block) = resolve_match_block(&a.name) else {
                v.state = AdapterState::Deferred;
                v.deferred_reason = Some("no stable device path (udev ID_PATH) to match on".into());
                tracing::warn!(iface = %a.name, "MAC pin deferred: no stable device path");
                return v;
            };
            match write_pin_link(layout, key, &match_block, &mac) {
                Ok(path) => {
                    v.state = AdapterState::Pinned;
                    v.source = Some(source);
                    v.pinned_mac = Some(mac);
                    v.link_file = Some(path.to_string_lossy().to_string());
                    tracing::info!(
                        iface = %a.name, key, mac = %mac,
                        "pinned a stable MAC on a no-efuse adapter (next boot)"
                    );
                }
                Err(e) => {
                    v.state = AdapterState::Deferred;
                    v.deferred_reason = Some(format!("link write failed: {e}"));
                    tracing::warn!(iface = %a.name, error = %e, "MAC pin link write failed");
                }
            }
        }
    }
    v
}
