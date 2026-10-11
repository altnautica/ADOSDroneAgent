//! What a config write must do beyond the file itself.
//!
//! Four lanes mirror a config change onto the node, and every config writer
//! (the native `PUT /api/config`, and the Python writer through
//! `ados-control --apply-config-side-effects`) runs them through [`apply`]:
//!
//! * **CRSF** (`radio.crsf`): the `crsf-enabled` marker the `ados-crsf` unit
//!   gates on mirrors `enabled`. A slice change kicks an enabled lane with
//!   `reload-or-restart` and stops a disabled one; a change to the router's
//!   view of the slice (the pinned device, the MAVLink-over-ELRS source, the
//!   command direction) `try-restart`s `ados-mavlink`, which loads that view
//!   once at startup.
//! * **Config tunnel** (`radio.tunnel`): the `tunnel-enabled` marker mirrors
//!   `enabled`; a slice change kicks `ados-tunnel-config` with
//!   `reload-or-restart`.
//! * **Setup AP** (`network.hotspot`): the `hotspot-enabled` marker mirrors
//!   `enabled`; a slice change `try-restart`s `ados-hostapd` and either
//!   `reload-or-restart`s `ados-dnsmasq-gs` (enabled) or clears its latched
//!   failure and stops it (disabled).
//! * **Remote access** (`remote_access`): when the tunnel posture or the unit
//!   name changes, `enable --now` or `disable --now` the tunnel unit (a renamed
//!   unit stops the old one first). Only names starting with `cloudflared` are
//!   acted on, so a config value cannot stop an arbitrary unit.
//!
//! Markers are reconciled on every write (idempotent, self-healing); units are
//! kicked only when their own slice changed. Everything is best-effort: a marker
//! or systemctl failure is recorded in the report and never fails a write that
//! already landed.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

/// The outcome of one side-effect pass.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct SideEffectReport {
    /// Markers created or removed, by file name.
    pub markers_changed: Vec<String>,
    /// Every command issued, as one space-joined line each.
    pub commands: Vec<String>,
    /// Marker IO errors, refused unit names and commands that failed.
    pub failures: Vec<String>,
}

/// Bring markers and units in line with a write from `old` to `new` (raw
/// config documents; a non-mapping reads as empty). Markers live in `etc_dir`.
/// `run` issues each unit command and reports whether it succeeded; the caller
/// owns how a command is bounded (the route runs them as timed async probes).
pub fn apply(
    old: &Value,
    new: &Value,
    etc_dir: &Path,
    run: &dyn Fn(&str, &[&str]) -> bool,
) -> SideEffectReport {
    let mut fx = Effects {
        etc_dir,
        run,
        report: SideEffectReport::default(),
    };
    fx.crsf(old, new);
    fx.tunnel(old, new);
    fx.hotspot(old, new);
    fx.remote_access(old, new);
    fx.report
}

struct Effects<'a> {
    etc_dir: &'a Path,
    run: &'a dyn Fn(&str, &[&str]) -> bool,
    report: SideEffectReport,
}

static EMPTY: Value = Value::Null;

/// The mapping at `path` of a raw document, or null when any level is absent
/// or not a mapping (compared as "empty").
fn slice<'v>(doc: &'v Value, path: &[&str]) -> &'v Value {
    let found = path
        .iter()
        .try_fold(doc, |node, key| node.as_object().and_then(|m| m.get(*key)));
    match found {
        Some(v) if v.is_object() => v,
        _ => &EMPTY,
    }
}

/// Python truthiness of a config value; absent is false.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn enabled(slice: &Value) -> bool {
    truthy(slice.get("enabled"))
}

/// The projection of `radio.crsf` the MAVLink router consumes: the pinned
/// device, the resolved MAVLink-over-ELRS source and the command direction.
fn router_view(crsf: &Value) -> (Value, Option<&'static str>, bool) {
    let device = crsf.get("device").cloned().unwrap_or(Value::Null);
    if enabled(crsf) && crsf.get("mode").and_then(Value::as_str) == Some("mavlink") {
        let source = match crsf.get("mavlink_transport").and_then(Value::as_str) {
            Some("backpack_wifi") => "backpack_wifi",
            _ => "serial",
        };
        (
            device,
            Some(source),
            truthy(crsf.get("mavlink_command_enabled")),
        )
    } else {
        (device, None, false)
    }
}

/// `(tunnel wanted, unit name)`; an absent block is provider `none`, unit
/// `cloudflared`.
fn posture(doc: &Value) -> (bool, String) {
    let remote = slice(doc, &["remote_access"]);
    let cf = slice(remote, &["cloudflare"]);
    let unit = cf
        .get("service_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("cloudflared")
        .to_string();
    let wanted = remote.get("provider").and_then(Value::as_str) == Some("cloudflare")
        && cf.get("enabled") == Some(&Value::Bool(true));
    (wanted, unit)
}

/// A tunnel unit name this module may act on.
pub fn tunnel_unit_allowed(unit: &str) -> bool {
    unit.strip_prefix("cloudflared").is_some_and(|rest| {
        rest.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._-".contains(c))
    })
}

impl Effects<'_> {
    fn systemctl(&mut self, args: &[&str]) {
        let line = format!("systemctl {}", args.join(" "));
        if !(self.run)("systemctl", args) {
            tracing::debug!(command = %line, "config side-effect command failed");
            self.report.failures.push(line.clone());
        }
        self.report.commands.push(line);
    }

    /// Mirror `wanted` onto the marker; true when its presence changed.
    fn marker(&mut self, name: &str, wanted: bool) -> bool {
        let path = self.etc_dir.join(name);
        let exists = path.exists();
        let result = match (wanted, exists) {
            (true, false) => std::fs::write(&path, b""),
            (false, true) => std::fs::remove_file(&path),
            _ => return false,
        };
        match result {
            Ok(()) => {
                tracing::info!(marker = name, present = wanted, "config marker reconciled");
                self.report.markers_changed.push(name.to_string());
                true
            }
            Err(e) => {
                tracing::warn!(marker = name, error = %e, "config marker reconcile failed");
                self.report.failures.push(format!("{name}: {e}"));
                false
            }
        }
    }

    fn crsf(&mut self, old: &Value, new: &Value) {
        let (before, after) = (
            slice(old, &["radio", "crsf"]),
            slice(new, &["radio", "crsf"]),
        );
        let flipped = self.marker("crsf-enabled", enabled(after));
        if flipped || before != after {
            // A running unit is never re-checked against its marker condition,
            // and the unit's ExecReload is a SIGHUP, so `reload-or-restart`
            // would leave a disabled lane resident. Off means stop.
            let verb = if enabled(after) {
                "reload-or-restart"
            } else {
                "stop"
            };
            self.systemctl(&["--no-block", verb, "ados-crsf.service"]);
        }
        if router_view(before) != router_view(after) {
            self.systemctl(&["--no-block", "try-restart", "ados-mavlink.service"]);
        }
    }

    fn tunnel(&mut self, old: &Value, new: &Value) {
        let (before, after) = (
            slice(old, &["radio", "tunnel"]),
            slice(new, &["radio", "tunnel"]),
        );
        let flipped = self.marker("tunnel-enabled", enabled(after));
        if flipped || before != after {
            self.systemctl(&[
                "--no-block",
                "reload-or-restart",
                "ados-tunnel-config.service",
            ]);
        }
    }

    fn hotspot(&mut self, old: &Value, new: &Value) {
        let (before, after) = (
            slice(old, &["network", "hotspot"]),
            slice(new, &["network", "hotspot"]),
        );
        let flipped = self.marker("hotspot-enabled", enabled(after));
        if !(flipped || before != after) {
            return;
        }
        self.systemctl(&["--no-block", "try-restart", "ados-hostapd.service"]);
        if enabled(after) {
            self.systemctl(&["--no-block", "reload-or-restart", "ados-dnsmasq-gs.service"]);
        } else {
            // systemd keeps a unit's last failure until it is reset, so an AP
            // turned off would otherwise still show a failed unit.
            self.systemctl(&["--no-block", "reset-failed", "ados-dnsmasq-gs.service"]);
            self.systemctl(&["--no-block", "stop", "ados-dnsmasq-gs.service"]);
        }
    }

    fn remote_access(&mut self, old: &Value, new: &Value) {
        let (before, after) = (posture(old), posture(new));
        if before == after {
            return;
        }
        let ((was_wanted, old_unit), (wanted, unit)) = (before, after);
        if was_wanted && old_unit != unit {
            self.kick_tunnel(&old_unit, false);
        }
        self.kick_tunnel(&unit, wanted);
    }

    fn kick_tunnel(&mut self, unit: &str, wanted: bool) {
        if !tunnel_unit_allowed(unit) {
            tracing::warn!(unit, "remote-access unit name refused");
            self.report
                .failures
                .push(format!("remote-access unit refused: {unit}"));
            return;
        }
        let verb = if wanted { "enable" } else { "disable" };
        self.systemctl(&["--no-block", verb, "--now", unit]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    fn run_with(old: Value, new: Value, etc: &Path) -> (SideEffectReport, Vec<String>) {
        let calls = RefCell::new(Vec::new());
        let run = |p: &str, a: &[&str]| {
            calls.borrow_mut().push(format!("{p} {}", a.join(" ")));
            true
        };
        let report = apply(&old, &new, etc, &run);
        (report, calls.into_inner())
    }

    fn crsf(fields: Value) -> Value {
        json!({ "radio": { "crsf": fields } })
    }

    #[test]
    fn crsf_enable_creates_marker() {
        let etc = tempfile::tempdir().unwrap();
        let on = crsf(json!({"enabled": true}));
        let (report, calls) = run_with(json!({}), on.clone(), etc.path());
        assert!(etc.path().join("crsf-enabled").exists());
        assert_eq!(report.markers_changed, vec!["crsf-enabled"]);
        assert_eq!(
            calls,
            vec!["systemctl --no-block reload-or-restart ados-crsf.service"]
        );
        // Turning the lane off stops the unit: a reload would leave it resident,
        // since systemd never re-checks the marker condition of a running unit.
        let (_, calls) = run_with(on, crsf(json!({"enabled": false})), etc.path());
        assert!(!etc.path().join("crsf-enabled").exists());
        assert_eq!(calls, vec!["systemctl --no-block stop ados-crsf.service"]);
        // A slice edit while the lane stays off keeps it stopped.
        let (_, calls) = run_with(
            crsf(json!({"enabled": false})),
            crsf(json!({"enabled": false, "packet_rate_hz": 250})),
            etc.path(),
        );
        assert_eq!(calls, vec!["systemctl --no-block stop ados-crsf.service"]);
    }

    #[test]
    fn an_unrelated_write_kicks_nothing() {
        let etc = tempfile::tempdir().unwrap();
        let old = json!({"agent": {"name": "a"}});
        let new = json!({"agent": {"name": "b"}});
        let (report, calls) = run_with(old, new, etc.path());
        assert!(calls.is_empty());
        assert_eq!(report, SideEffectReport::default());
    }

    #[test]
    fn the_router_restarts_only_on_its_view() {
        let etc = tempfile::tempdir().unwrap();
        let lane = |extra: Value| {
            let mut f = json!({"enabled": true, "mode": "mavlink", "packet_rate_hz": 150});
            let fields = f.as_object_mut().unwrap();
            fields.extend(extra.as_object().unwrap().clone());
            crsf(f)
        };
        let rate = lane(json!({"packet_rate_hz": 250}));
        let (_, calls) = run_with(lane(json!({})), rate, etc.path());
        assert!(!calls.iter().any(|c| c.contains("ados-mavlink")));
        let wifi = lane(json!({"mavlink_transport": "backpack_wifi"}));
        let (_, calls) = run_with(lane(json!({})), wifi, etc.path());
        let router = "systemctl --no-block try-restart ados-mavlink.service";
        assert!(calls.iter().any(|c| c == router));
    }

    #[test]
    fn tunnel_and_hotspot_markers_follow_their_slices() {
        let etc = tempfile::tempdir().unwrap();
        let doc = |hotspot: bool| {
            json!({
                "radio": {"tunnel": {"enabled": true}},
                "network": {"hotspot": {"enabled": hotspot}},
            })
        };
        let (_, calls) = run_with(json!({}), doc(true), etc.path());
        assert!(etc.path().join("tunnel-enabled").exists());
        assert!(etc.path().join("hotspot-enabled").exists());
        for kick in [
            "systemctl --no-block reload-or-restart ados-tunnel-config.service",
            "systemctl --no-block reload-or-restart ados-dnsmasq-gs.service",
        ] {
            assert!(calls.iter().any(|c| c == kick), "{kick}");
        }
        let (_, calls) = run_with(doc(true), doc(false), etc.path());
        assert!(!etc.path().join("hotspot-enabled").exists());
        assert_eq!(
            calls,
            vec![
                "systemctl --no-block try-restart ados-hostapd.service",
                "systemctl --no-block reset-failed ados-dnsmasq-gs.service",
                "systemctl --no-block stop ados-dnsmasq-gs.service",
            ]
        );
    }

    #[test]
    fn remote_access_posture_enables_disables_and_refuses_foreign_units() {
        let etc = tempfile::tempdir().unwrap();
        let remote = |provider: &str, enabled: bool, unit: &str| {
            json!({"remote_access": {
                "provider": provider,
                "cloudflare": {"enabled": enabled, "service_name": unit},
            }})
        };
        let on = remote("cloudflare", true, "cloudflared");
        let (_, calls) = run_with(json!({}), on.clone(), etc.path());
        assert_eq!(calls, vec!["systemctl --no-block enable --now cloudflared"]);
        let off = remote("none", true, "cloudflared");
        let (_, calls) = run_with(on.clone(), off, etc.path());
        assert_eq!(
            calls,
            vec!["systemctl --no-block disable --now cloudflared"]
        );
        let renamed = remote("cloudflare", true, "cloudflared-b");
        let (_, calls) = run_with(on, renamed, etc.path());
        assert_eq!(
            calls,
            vec![
                "systemctl --no-block disable --now cloudflared",
                "systemctl --no-block enable --now cloudflared-b",
            ]
        );
        let foreign = remote("cloudflare", true, "sshd");
        let (report, calls) = run_with(json!({}), foreign, etc.path());
        assert!(calls.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(!tunnel_unit_allowed("cloudflared;rm"));
        assert!(tunnel_unit_allowed("cloudflared-tunnel.service"));
    }
}
