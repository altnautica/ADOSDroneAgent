//! The install-result contract: `/var/lib/ados/install-result.json`.
//!
//! This is a stable machine-readable file the agent heartbeat + Mission
//! Control read after every install. The key set, key order, and pretty-print
//! shape are a frozen wire contract — the bash installer wrote it with
//! `json.dump(result, indent=2)` + a trailing newline, and the Rust installer
//! must produce byte-identical output so existing consumers do not regress.

use std::path::Path;

use serde::Serialize;

/// The install outcome. Field order is load-bearing: serde_json serializes
/// struct fields in declaration order, and the contract pins that order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallResult {
    /// `ok` | `degraded` | `failed`.
    pub status: String,
    /// Installed agent version, or `unknown`.
    pub version: String,
    /// `drone` | `ground_station`.
    pub profile: String,
    /// Board id (override sentinel, device-tree model, or `unknown`).
    pub board: String,
    /// `uname -r`.
    #[serde(rename = "kernelRelease")]
    pub kernel_release: String,
    /// `prebuilt` | `dkms` | empty when no RTL adapter / driver.
    #[serde(rename = "wfbModuleSource")]
    pub wfb_module_source: String,
    /// Every step that did not succeed (required and optional).
    #[serde(rename = "failedSteps")]
    pub failed_steps: Vec<String>,
    /// The subset of `failedSteps` that were Required (hard failures).
    #[serde(rename = "requiredFailures")]
    pub required_failures: Vec<String>,
    /// UTC ISO-8601 second-precision timestamp.
    pub ts: String,
}

impl InstallResult {
    /// Render the contract JSON exactly as the consumers expect: 2-space
    /// pretty indent, declaration key order, with a single trailing newline.
    pub fn to_contract_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("InstallResult serializes");
        s.push('\n');
        s
    }

    /// Write the contract atomically: render to `<path>.tmp`, then rename over
    /// `path`, then set mode 0644. The rename is the commit point so a reader
    /// never observes a half-written file. On a non-Linux dev host the mode
    /// chmod is skipped (the contract is still written for inspection/tests).
    pub fn write_atomic(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = tmp_path(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&tmp, self.to_contract_json())?;
        set_mode_0644(&tmp);
        std::fs::rename(&tmp, path)?;
        set_mode_0644(path);
        Ok(())
    }
}

/// `<path>.tmp` sibling for the atomic write staging file.
fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    std::path::PathBuf::from(s)
}

#[cfg(target_os = "linux")]
fn set_mode_0644(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
}

#[cfg(not(target_os = "linux"))]
fn set_mode_0644(_path: &Path) {}

/// Accumulates step failures across a graph run, then classifies the overall
/// install status. `record(step, required)` is called once per failed step;
/// `derive_status()` collapses the two lists into the contract status string.
#[derive(Debug, Clone, Default)]
pub struct FailureAccumulator {
    /// Every failed step, in the order they failed.
    pub failed: Vec<String>,
    /// The subset of `failed` that were Required.
    pub required: Vec<String>,
}

impl FailureAccumulator {
    /// A fresh accumulator with no recorded failures.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one failed step. A required failure is appended to both lists.
    pub fn record(&mut self, step: &str, required: bool) {
        self.failed.push(step.to_string());
        if required {
            self.required.push(step.to_string());
        }
    }

    /// Classify the install: `failed` if any required step failed, else
    /// `degraded` if any optional step failed, else `ok`.
    pub fn derive_status(&self) -> &'static str {
        if !self.required.is_empty() {
            "failed"
        } else if !self.failed.is_empty() {
            "degraded"
        } else {
            "ok"
        }
    }
}

/// UTC ISO-8601, second precision: `YYYY-MM-DDTHH:MM:SSZ`.
pub fn now_iso8601_utc() -> String {
    use time::format_description::well_known::Iso8601;
    use time::OffsetDateTime;

    // Second-precision, Z-suffixed. The macro config drops sub-second digits
    // and forces the literal `Z` so the string matches the bash
    // `date -u +%Y-%m-%dT%H:%M:%SZ` form exactly.
    const CFG: Iso8601<
        {
            time::format_description::well_known::iso8601::Config::DEFAULT
                .set_time_precision(time::format_description::well_known::iso8601::TimePrecision::Second {
                    decimal_digits: None,
                })
                .encode()
        },
    > = Iso8601;

    OffsetDateTime::now_utc()
        .format(&CFG)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The frozen contract: 2-space pretty indent, declaration key order,
    // single trailing newline — byte-identical to the bash installer's
    // `json.dump(result, indent=2)` + `fh.write("\n")`.
    const GOLDEN: &str = "{\n  \"status\": \"ok\",\n  \"version\": \"0.40.4\",\n  \"profile\": \"drone\",\n  \"board\": \"Raspberry Pi 4 Model B\",\n  \"kernelRelease\": \"6.6.20-v8+\",\n  \"wfbModuleSource\": \"dkms\",\n  \"failedSteps\": [],\n  \"requiredFailures\": [],\n  \"ts\": \"2026-05-31T12:00:00Z\"\n}\n";

    fn golden_result() -> InstallResult {
        InstallResult {
            status: "ok".to_string(),
            version: "0.40.4".to_string(),
            profile: "drone".to_string(),
            board: "Raspberry Pi 4 Model B".to_string(),
            kernel_release: "6.6.20-v8+".to_string(),
            wfb_module_source: "dkms".to_string(),
            failed_steps: vec![],
            required_failures: vec![],
            ts: "2026-05-31T12:00:00Z".to_string(),
        }
    }

    #[test]
    fn install_result_matches_golden_json() {
        let got = golden_result().to_contract_json();
        assert_eq!(got, GOLDEN, "install-result JSON drifted from the contract");
    }

    #[test]
    fn write_atomic_lands_golden_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("install-result.json");
        golden_result().write_atomic(&path).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, GOLDEN);
        // The staging tmp must not linger after the rename commit.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn accumulator_classifies_ok() {
        let acc = FailureAccumulator::new();
        assert_eq!(acc.derive_status(), "ok");
    }

    #[test]
    fn accumulator_classifies_degraded_on_optional_only() {
        let mut acc = FailureAccumulator::new();
        acc.record("radio-driver", false);
        assert_eq!(acc.derive_status(), "degraded");
        assert_eq!(acc.failed, vec!["radio-driver".to_string()]);
        assert!(acc.required.is_empty());
    }

    #[test]
    fn accumulator_classifies_failed_on_required() {
        let mut acc = FailureAccumulator::new();
        acc.record("radio-driver", false);
        acc.record("systemd", true);
        assert_eq!(acc.derive_status(), "failed");
        assert_eq!(acc.required, vec!["systemd".to_string()]);
        assert_eq!(acc.failed.len(), 2);
    }

    #[test]
    fn now_iso8601_utc_is_second_precision_z() {
        let ts = now_iso8601_utc();
        // YYYY-MM-DDTHH:MM:SSZ — 20 chars, ends in Z, no sub-second '.'.
        assert_eq!(ts.len(), 20, "got {ts}");
        assert!(ts.ends_with('Z'), "got {ts}");
        assert!(!ts.contains('.'), "got {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }
}
