//! Data-cap traffic shaping with a root tbf qdisc.

use super::*;

impl ShareUplinkFirewall {
    async fn tc_add_throttle(&self, iface: &str, rate_kbps: u32) -> Option<String> {
        // Delete any existing root qdisc first so repeated calls converge.
        // Absence is fine; ignore the result.
        self.runner
            .run(&["tc", "qdisc", "del", "dev", iface, "root"], CMD_TIMEOUT)
            .await;
        let rate = format!("{rate_kbps}kbit");
        let out = self
            .runner
            .run(
                &[
                    "tc", "qdisc", "add", "dev", iface, "root", "tbf", "rate", &rate, "burst",
                    "32kbit", "latency", "400ms",
                ],
                CMD_TIMEOUT,
            )
            .await;
        (!out.ok()).then(|| non_empty(&out.stderr, "tc_add_failed"))
    }

    async fn tc_remove_throttle(&self, iface: &str) -> Option<String> {
        let out = self
            .runner
            .run(&["tc", "qdisc", "del", "dev", iface, "root"], CMD_TIMEOUT)
            .await;
        let low = out.stderr.to_lowercase();
        if !out.ok() && !low.contains("no such") && !low.contains("cannot find") {
            return Some(non_empty(&out.stderr, "tc_remove_failed"));
        }
        None
    }

    /// The root tbf on `iface`: `None` when there is none, `Some(rate)` when
    /// there is one (`rate` is `None` when it could not be parsed).
    async fn tc_root_tbf(&self, iface: &str) -> Option<Option<u32>> {
        let out = self
            .runner
            .run(&["tc", "qdisc", "show", "dev", iface, "root"], CMD_TIMEOUT)
            .await;
        if !out.ok() {
            return None;
        }
        parse_root_tbf(&out.stdout)
    }

    /// Hold the cellular iface to the rate the cap state needs, and clear a
    /// previous cellular iface's shaping when the modem iface changed.
    pub(super) async fn sync_shaping(
        &self,
        applied: &mut Applied,
        intent: &FirewallIntent,
    ) -> Option<String> {
        let target = intent.cellular_iface.as_deref().filter(|s| !s.is_empty());
        if let Some(prev) = applied.shaped_iface.clone() {
            if Some(prev.as_str()) != target {
                self.tc_remove_throttle(&prev).await;
                applied.shaped_iface = None;
            }
        }
        let iface = target?;
        let current = self.tc_root_tbf(iface).await;
        match intent.shaping_rate_kbps() {
            None => {
                applied.shaped_iface = None;
                if current.is_some() {
                    return self.tc_remove_throttle(iface).await;
                }
                None
            }
            Some(rate) => {
                if current == Some(Some(rate)) {
                    applied.shaped_iface = Some(iface.to_string());
                    return None;
                }
                let err = self.tc_add_throttle(iface, rate).await;
                if err.is_none() {
                    applied.shaped_iface = Some(iface.to_string());
                }
                err
            }
        }
    }
}

/// Parse `tc qdisc show dev <iface> root`: `None` when the root is not a tbf,
/// `Some(rate_kbps)` when it is (`None` inside when the rate is not in kbit).
pub(super) fn parse_root_tbf(stdout: &str) -> Option<Option<u32>> {
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("qdisc tbf "))?;
    let mut tokens = line.split_whitespace();
    let rate = tokens
        .by_ref()
        .skip_while(|t| *t != "rate")
        .nth(1)
        .and_then(|r| {
            let lower = r.to_ascii_lowercase();
            lower.strip_suffix("kbit")?.parse::<u32>().ok()
        });
    Some(rate)
}
