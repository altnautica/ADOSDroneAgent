//! Parse the `storage.janitor` block of `config.yaml`.

use super::*;

/// Parse `storage.janitor`. Absent or malformed → enabled defaults, matching
/// every other reconciler here: a config the box cannot read must not be the
/// reason a safety net is off.
pub fn read_config_from(text: &str) -> JanitorConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        storage: Storage,
    }
    #[derive(serde::Deserialize, Default)]
    struct Storage {
        #[serde(default)]
        janitor: Option<Janitor>,
    }
    #[derive(serde::Deserialize)]
    struct Janitor {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        interval_s: Option<u64>,
        #[serde(default)]
        pressure_free_pct: Option<f64>,
        #[serde(default)]
        critical_free_pct: Option<f64>,
        #[serde(default)]
        plugin_log_max_mb: Option<u64>,
        #[serde(default)]
        plugin_log_keep_mb: Option<u64>,
        #[serde(default)]
        audit_max_mb: Option<u64>,
        #[serde(default)]
        audit_keep_mb: Option<u64>,
        #[serde(default)]
        recording_retention_days: Option<u64>,
        #[serde(default)]
        recording_pressure_retention_days: Option<u64>,
        #[serde(default)]
        recording_keep_newest: Option<usize>,
        #[serde(default)]
        journal_pressure_mb: Option<u64>,
        #[serde(default)]
        journal_floor_mb: Option<u64>,
        #[serde(default)]
        quarantine_keep_newest: Option<usize>,
        #[serde(default)]
        budget_mb: Option<u64>,
        #[serde(default)]
        caps: Option<RawCaps>,
    }
    #[derive(serde::Deserialize)]
    struct RawCaps {
        #[serde(default)]
        log_store_mb: Option<u64>,
        #[serde(default)]
        quarantined_stores_mb: Option<u64>,
        #[serde(default)]
        recordings_mb: Option<u64>,
        #[serde(default)]
        plugin_logs_mb: Option<u64>,
        #[serde(default)]
        audit_log_mb: Option<u64>,
        #[serde(default)]
        journal_mb: Option<u64>,
        #[serde(default)]
        apt_mb: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    const MB: u64 = 1024 * 1024;

    let d = JanitorConfig::default();
    let Ok(raw) = serde_norway::from_str::<Raw>(text) else {
        return d;
    };
    let Some(j) = raw.storage.janitor else {
        return d;
    };
    JanitorConfig {
        enabled: j.enabled,
        interval: Duration::from_secs(j.interval_s.unwrap_or(plan::DEFAULT_INTERVAL_S).max(60)),
        pressure_free_pct: j.pressure_free_pct.unwrap_or(d.pressure_free_pct),
        critical_free_pct: j.critical_free_pct.unwrap_or(d.critical_free_pct),
        plugin_log_max_bytes: j
            .plugin_log_max_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.plugin_log_max_bytes),
        plugin_log_keep_bytes: j
            .plugin_log_keep_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.plugin_log_keep_bytes),
        audit_max_bytes: j
            .audit_max_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.audit_max_bytes),
        audit_keep_bytes: j
            .audit_keep_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.audit_keep_bytes),
        recording_retention: j
            .recording_retention_days
            .map(|days| Duration::from_secs(days.saturating_mul(86_400)))
            .unwrap_or(d.recording_retention),
        recording_pressure_retention: j
            .recording_pressure_retention_days
            .map(|days| Duration::from_secs(days.saturating_mul(86_400)))
            .unwrap_or(d.recording_pressure_retention),
        recording_keep_newest: j.recording_keep_newest.unwrap_or(d.recording_keep_newest),
        journal_pressure_bytes: j
            .journal_pressure_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.journal_pressure_bytes),
        journal_floor_bytes: j
            .journal_floor_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.journal_floor_bytes),
        quarantine_keep_newest: j.quarantine_keep_newest.unwrap_or(d.quarantine_keep_newest),
        budget_bytes: j
            .budget_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.budget_bytes),
        caps: match j.caps {
            None => d.caps,
            Some(c) => budget::Caps {
                log_store: c.log_store_mb.map(|m| m * MB).unwrap_or(d.caps.log_store),
                quarantined_stores: c
                    .quarantined_stores_mb
                    .map(|m| m * MB)
                    .unwrap_or(d.caps.quarantined_stores),
                recordings: c.recordings_mb.map(|m| m * MB).unwrap_or(d.caps.recordings),
                plugin_logs: c
                    .plugin_logs_mb
                    .map(|m| m * MB)
                    .unwrap_or(d.caps.plugin_logs),
                audit_log: c.audit_log_mb.map(|m| m * MB).unwrap_or(d.caps.audit_log),
                journal: c.journal_mb.map(|m| m * MB).unwrap_or(d.caps.journal),
                apt: c.apt_mb.map(|m| m * MB).unwrap_or(d.caps.apt),
            },
        },
    }
}
