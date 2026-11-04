//! The install's `--profile` written into `config.yaml`.
//!
//! Every agent unit resolves its profile from `agent.profile` first and falls
//! back to `profile.conf` only when that key is absent or `auto`. An install
//! with an explicit `--profile` over a config naming another profile rewrote
//! `profile.conf` and switched the unit set, but the supervisor and the rest
//! of the node kept running as the old profile: a ground station reinstalled
//! as a drone applied its ground-station role at boot and never started drone
//! video. So an explicit `--profile` also rewrites that one key. An absent key
//! or `auto` already defers to `profile.conf` and is left alone, and an install
//! without `--profile` never touches it. The edit is line-level, so comments
//! and layout survive, and it runs under the shared config lock.

use std::path::Path;

use super::arbitration::{acquire_lock, indent_of};

const KEY: &str = "profile:";

/// The `agent.profile` value of a `profile: value  # comment` line, comment
/// and quotes stripped, in the underscore form.
fn value_of(line: &str) -> String {
    let rest = line.trim_start()[KEY.len()..].trim();
    let rest = rest.split(" #").next().unwrap_or(rest).trim();
    rest.trim_matches(|c| c == '"' || c == '\'')
        .replace('-', "_")
}

/// Point `agent.profile` at `profile` in a config body (pure). `None` when the
/// key already names it, is absent or `auto`, or the `agent` block is not in
/// block style.
pub fn apply_to_body(body: &str, profile: &str) -> Option<String> {
    let lines: Vec<&str> = body.lines().collect();
    let header = lines.iter().position(|l| {
        l.starts_with("agent:") && {
            let rest = l["agent:".len()..].trim();
            rest.is_empty() || rest.starts_with('#')
        }
    })?;
    let end = lines[header + 1..]
        .iter()
        .position(|l| indent_of(l) == Some(0))
        .map_or(lines.len(), |p| header + 1 + p);
    let child_indent = lines[header + 1..end]
        .iter()
        .copied()
        .filter_map(indent_of)
        .find(|&i| i > 0)?;
    let i = (header + 1..end).find(|&i| {
        indent_of(lines[i]) == Some(child_indent) && lines[i].trim_start().starts_with(KEY)
    })?;

    let current = value_of(lines[i]);
    if current.is_empty() || current == "auto" || current == profile {
        return None;
    }
    // A trailing comment is kept with the spacing before it.
    let comment = lines[i]
        .find(" #")
        .map(|c| &lines[i][lines[i][..c].trim_end().len()..])
        .unwrap_or_default();
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    out[i] = format!("{}{KEY} \"{profile}\"{comment}", " ".repeat(child_indent));
    let mut joined = out.join("\n");
    joined.push('\n');
    Some(joined)
}

/// Point `agent.profile` in the config at `config` at `profile`, under the
/// lock at `lock_path`. `Ok(true)` when the file changed; an absent config is
/// left alone.
pub fn apply(config: &Path, lock_path: &Path, profile: &str) -> Result<bool, String> {
    if !config.is_file() {
        return Ok(false);
    }
    let _lock = acquire_lock(lock_path)?;
    let body = std::fs::read_to_string(config).map_err(|e| e.to_string())?;
    let Some(updated) = apply_to_body(&body, profile) else {
        return Ok(false);
    };
    crate::env::write_atomic_durable(config, updated.as_bytes(), Some(0o600))
        .map_err(|e| e.to_string())?;
    tracing::info!(
        profile,
        "pointed the config's agent profile at the install's"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GS: &str = "# header\nagent:\n  device_id: abcd\n  profile: \"ground_station\"  # set by the installer\n  name: n\n\nmavlink:\n  profile: keep\n";

    fn profile_of(body: &str) -> String {
        let parsed: serde_norway::Value = serde_norway::from_str(body).unwrap();
        parsed["agent"]["profile"].as_str().unwrap().to_string()
    }

    /// Regression: a ground station reinstalled with `--profile drone` kept
    /// `agent.profile: ground_station`, which every unit reads before
    /// `profile.conf`, so the node went on running as a ground station.
    #[test]
    fn an_explicit_switch_rewrites_the_profile_key_only() {
        let out = apply_to_body(GS, "drone").expect("changed");
        assert_eq!(profile_of(&out), "drone");
        assert!(out.contains("  profile: \"drone\"  # set by the installer\n"));
        assert!(out.starts_with("# header\nagent:\n  device_id: abcd\n"));
        assert!(out.contains("  name: n\n"));
        // A same-named key in another block is not the agent's.
        assert!(out.contains("mavlink:\n  profile: keep\n"));
        // Idempotent, and the reverse switch works the same way.
        assert_eq!(apply_to_body(&out, "drone"), None);
        assert_eq!(
            profile_of(&apply_to_body(&out, "ground_station").unwrap()),
            "ground_station"
        );
    }

    #[test]
    fn spellings_auto_and_absent_keys_are_left_alone() {
        // The hyphen spelling names the same profile.
        assert_eq!(
            apply_to_body("agent:\n  profile: ground-station\n", "ground_station"),
            None
        );
        // `auto` and an absent key already defer to profile.conf.
        assert_eq!(apply_to_body("agent:\n  profile: auto\n", "drone"), None);
        assert_eq!(apply_to_body("agent:\n  name: n\n", "drone"), None);
        assert_eq!(apply_to_body("video:\n  mode: auto\n", "drone"), None);
        // Flow style is not edited.
        assert_eq!(apply_to_body("agent: {profile: drone}\n", "visor"), None);
    }

    #[test]
    fn the_config_on_disk_is_rewritten_under_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let lock = dir.path().join("config.yaml.lock");
        assert_eq!(apply(&config, &lock, "drone"), Ok(false));
        std::fs::write(&config, GS).unwrap();
        assert_eq!(apply(&config, &lock, "drone"), Ok(true));
        assert_eq!(
            profile_of(&std::fs::read_to_string(&config).unwrap()),
            "drone"
        );
        assert_eq!(apply(&config, &lock, "drone"), Ok(false));
    }
}
