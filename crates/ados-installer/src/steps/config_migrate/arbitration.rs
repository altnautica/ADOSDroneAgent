//! The install-time injector-arbitration policy for `config.yaml`.
//!
//! A drone has a single MAVLink injector, so `mavlink.injector_arbitration:
//! true` there is a leftover of an older template and is removed. A ground
//! station arbitrates between its injectors, so it gains the key when the
//! config does not set it at all; an explicit value is the operator's and is
//! kept. The edit is line-level, so comments and layout survive, and it runs
//! under the shared config lock with a bounded wait.

use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

use super::{LOCK_RETRY, LOCK_WAIT_ATTEMPTS};

const KEY: &str = "injector_arbitration:";

/// Indentation width of `line`, or `None` for a blank or comment-only line.
fn indent_of(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    Some(line.len() - trimmed.len())
}

/// The value part of a `key: value  # comment` line, comment and quotes
/// stripped.
fn scalar_after_key(line: &str) -> &str {
    let rest = line.trim_start()[KEY.len()..].trim();
    let rest = rest.split(" #").next().unwrap_or(rest).trim();
    rest.trim_matches(|c| c == '"' || c == '\'')
}

/// Apply the policy for `profile` to a config body (pure). `None` when the body
/// already conforms or its `mavlink` block is not in block style.
pub fn apply_to_body(body: &str, profile: &str) -> Option<String> {
    let lines: Vec<&str> = body.lines().collect();
    let header = lines.iter().position(|l| {
        l.starts_with("mavlink:") && {
            let rest = l["mavlink:".len()..].trim();
            rest.is_empty() || rest.starts_with('#')
        }
    });
    let inline_mavlink = header.is_none()
        && lines
            .iter()
            .any(|l| l.starts_with("mavlink:") && !l["mavlink:".len()..].trim().is_empty());
    if inline_mavlink {
        return None;
    }

    // The block runs from the header to the next top-level key.
    let (start, end) = match header {
        Some(h) => {
            let end = lines[h + 1..]
                .iter()
                .position(|l| indent_of(l) == Some(0))
                .map_or(lines.len(), |p| h + 1 + p);
            (h + 1, end)
        }
        None => (lines.len(), lines.len()),
    };
    let child_indent = lines[start..end]
        .iter()
        .copied()
        .filter_map(indent_of)
        .find(|&i| i > 0);
    let key_line = (start..end).find(|&i| {
        Some(lines[i].len() - lines[i].trim_start().len()) == child_indent
            && lines[i].trim_start().starts_with(KEY)
    });

    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    match profile {
        "drone" => {
            let i = key_line?;
            if scalar_after_key(lines[i]) != "true" {
                return None;
            }
            out.remove(i);
        }
        "ground_station" => {
            if key_line.is_some() {
                return None;
            }
            let line = format!("{}{KEY} true", " ".repeat(child_indent.unwrap_or(2)));
            match header {
                Some(h) => out.insert(h + 1, line),
                None => {
                    out.push("mavlink:".to_string());
                    out.push(line);
                }
            }
        }
        _ => return None,
    }
    let mut joined = out.join("\n");
    joined.push('\n');
    Some(joined)
}

/// Take the exclusive config lock, retrying [`LOCK_WAIT_ATTEMPTS`] times
/// [`LOCK_RETRY`] apart. The lock releases when the returned file drops.
fn acquire_lock(lock_path: &Path) -> Result<File, String> {
    if let Some(parent) = lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + LOCK_RETRY * LOCK_WAIT_ATTEMPTS;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "config lock {} held for {}s",
                        lock_path.display(),
                        (LOCK_RETRY * LOCK_WAIT_ATTEMPTS).as_secs()
                    ));
                }
                std::thread::sleep(LOCK_RETRY.min(Duration::from_millis(250)));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.to_string()),
        }
    }
}

/// Apply the policy for `profile` to the config at `config`, under the lock at
/// `lock_path`. `Ok(true)` when the file changed; an absent config is left
/// alone.
pub fn apply_policy(config: &Path, lock_path: &Path, profile: &str) -> Result<bool, String> {
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
        "applied the injector arbitration policy to the config"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRONE: &str = "# header\nagent:\n  profile: \"drone\"\n\nmavlink:\n  serial_port: \"\"\n  component_id: 191\n  injector_arbitration: true\n\nlogging:\n  level: \"info\"\n";

    #[test]
    fn migration_drone_drops_arbitration() {
        let out = apply_to_body(DRONE, "drone").expect("changed");
        assert!(!out.contains("injector_arbitration"));
        assert!(out.contains("# header\n"));
        assert!(out.contains("  component_id: 191\n"));
        assert!(out.contains("logging:\n  level: \"info\"\n"));
        // Idempotent, and an explicit `false` is the operator's.
        assert_eq!(apply_to_body(&out, "drone"), None);
        let explicit = DRONE.replace("arbitration: true", "arbitration: false");
        assert_eq!(apply_to_body(&explicit, "drone"), None);
    }

    #[test]
    fn migration_gs_inserts_arbitration_when_absent() {
        let gs = DRONE.replace("  injector_arbitration: true\n", "");
        let out = apply_to_body(&gs, "ground_station").expect("changed");
        let parsed: serde_norway::Value = serde_norway::from_str(&out).unwrap();
        assert_eq!(
            parsed["mavlink"]["injector_arbitration"].as_bool(),
            Some(true)
        );
        assert_eq!(apply_to_body(&out, "ground_station"), None);
        // An explicit false on a ground station is kept.
        let explicit = DRONE.replace("arbitration: true", "arbitration: false");
        assert_eq!(apply_to_body(&explicit, "ground_station"), None);
        // No mavlink block at all: one is added.
        let bare = apply_to_body("agent:\n  profile: ground_station\n", "ground_station").unwrap();
        let parsed: serde_norway::Value = serde_norway::from_str(&bare).unwrap();
        assert_eq!(
            parsed["mavlink"]["injector_arbitration"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn other_profiles_and_flow_style_are_untouched() {
        assert_eq!(apply_to_body(DRONE, "workstation"), None);
        assert_eq!(
            apply_to_body("mavlink: {injector_arbitration: true}\n", "drone"),
            None
        );
    }

    #[test]
    fn a_briefly_held_config_lock_is_waited_out() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let lock = dir.path().join("config.yaml.lock");
        std::fs::write(&config, DRONE).unwrap();

        let holder = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)
            .unwrap();
        holder.lock().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            drop(holder);
        });

        assert_eq!(apply_policy(&config, &lock, "drone"), Ok(true));
        release.join().unwrap();
        assert!(!std::fs::read_to_string(&config)
            .unwrap()
            .contains("injector_arbitration"));
    }
}
