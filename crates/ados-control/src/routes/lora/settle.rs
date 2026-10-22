//! Waiting for the lane to report a written change, so `PUT /api/v1/lora`
//! answers with the body the new settings produce, never the pre-change one.

use std::path::Path;
use std::time::{Duration, SystemTime};

use ados_lora_governor::regions;
use serde_json::Value;

use super::status_body;

/// How long the lane gets to rebuild and rewrite its sidecar after a reload.
pub(super) const SETTLE_TIMEOUT: Duration = Duration::from_secs(8);
const POLL: Duration = Duration::from_millis(200);

/// The values a written change must show in the lane body.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Expect {
    /// `Some(code)` when the region changed: the plan code it resolves to
    /// (`None` inside when nothing resolves, so `region` reads null).
    region: Option<Option<&'static str>>,
    freq_mhz: Option<f64>,
    erp_mw: Option<f64>,
    sf: Option<u64>,
}

impl Expect {
    pub(crate) fn from_change(change: &[(&'static str, Value)], country: Option<&str>) -> Self {
        let get = |k: &str| change.iter().find(|(key, _)| *key == k).map(|(_, v)| v);
        Self {
            region: get("region")
                .map(|v| regions::resolve(v.as_str(), country).map(|r| r.plan.code)),
            freq_mhz: get("freq_mhz").and_then(Value::as_f64),
            erp_mw: get("erp_mw").and_then(Value::as_f64),
            sf: get("sf").and_then(Value::as_u64),
        }
    }

    /// Whether a present lane body reflects every changed value.
    pub(crate) fn reflected_by(&self, body: &Value) -> bool {
        let close = |v: &Value, want: f64| v.as_f64().is_some_and(|x| (x - want).abs() < 1e-6);
        let region_ok = self.region.is_none_or(|want| match want {
            Some(code) => body["region"]["code"] == code,
            None => body["region"].is_null(),
        });
        let freq_ok = self
            .freq_mhz
            .is_none_or(|f| close(&body["channel"]["freq_mhz"], f));
        let sf_ok = self.sf.is_none_or(|s| body["channel"]["sf"] == s);
        let erp_ok = self.erp_mw.is_none_or(|e| {
            body["governor"]["rules"].as_array().is_some_and(|rules| {
                rules
                    .iter()
                    .any(|r| r["kind"] == "max_power" && close(&r["configured_mw"], e))
            })
        });
        body["present"] == true && region_ok && freq_ok && sf_ok && erp_ok
    }
}

/// Poll the lane until it reflects `expect`. Returns the GET body, or `None`
/// when the lane is running but has not reported the change in time. A lane
/// that is not running answers at once with its `present: false` body.
pub(super) async fn settled_body(
    run_dir: &Path,
    expect: &Expect,
    timeout: Duration,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let body = status_body(run_dir, SystemTime::now());
        if body["present"] != true || expect.reflected_by(&body) {
            return Some(body);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn lane(region: Value, freq: f64, sf: u64, erp: f64) -> Value {
        json!({
            "present": true,
            "region": region,
            "channel": {"freq_mhz": freq, "sf": sf, "bw_khz": 125},
            "governor": {"rules": [{"kind": "max_power", "configured_mw": erp}]},
        })
    }

    #[test]
    fn only_a_body_carrying_every_changed_value_counts() {
        let change = [("region", json!("EU")), ("sf", json!(9))];
        let want = Expect::from_change(&change, None);
        let old = lane(json!({"code": "US"}), 915.0, 7, 25.0);
        assert!(!want.reflected_by(&old));
        let half = lane(json!({"code": "EU"}), 868.1, 7, 25.0);
        assert!(!want.reflected_by(&half));
        let new = lane(json!({"code": "EU"}), 868.1, 9, 25.0);
        assert!(want.reflected_by(&new));
    }

    #[test]
    fn power_and_frequency_are_matched_against_the_lane() {
        let change = [("freq_mhz", json!(868.3)), ("erp_mw", json!(10.0))];
        let want = Expect::from_change(&change, None);
        assert!(!want.reflected_by(&lane(json!(null), 868.1, 7, 10.0)));
        assert!(!want.reflected_by(&lane(json!(null), 868.3, 7, 25.0)));
        assert!(want.reflected_by(&lane(json!(null), 868.3, 7, 10.0)));
    }

    #[test]
    fn a_cleared_region_without_a_country_expects_no_region() {
        let want = Expect::from_change(&[("region", Value::Null)], None);
        assert!(want.reflected_by(&lane(json!(null), 868.1, 7, 25.0)));
        assert!(!want.reflected_by(&lane(json!({"code": "EU"}), 868.1, 7, 25.0)));
    }

    #[tokio::test]
    async fn an_absent_lane_answers_at_once_and_a_stale_one_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let want = Expect::from_change(&[("sf", json!(9))], None);
        let body = settled_body(dir.path(), &want, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(body["present"], false);
        std::fs::write(
            dir.path().join("lora.json"),
            r#"{"v":1,"region":null,"channel":{"sf":7,"bw_khz":125},"governor":{"rules":[]}}"#,
        )
        .unwrap();
        assert!(settled_body(dir.path(), &want, Duration::ZERO)
            .await
            .is_none());
    }
}
