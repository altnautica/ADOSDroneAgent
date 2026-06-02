//! Regulatory-domain reader (`iw reg get`).
//!
//! The kernel maps each wiphy's permitted channel set and per-channel transmit
//! power ceiling from the active regulatory domain. A radio brought up under the
//! wrong domain runs silently power-capped on a band the domain forbids, which is
//! exactly the failure that makes a transmit counter advance while no energy
//! reaches a receiver. Recording the active domain over time turns "which
//! country was the kernel enforcing when the link went dark" from a live
//! reproduction into a query.
//!
//! `iw reg get` prints the global domain first, then one block per self-managed
//! phy. Each block opens with a `country XX: DFS-REGION` line, e.g.
//!
//! ```text
//! global
//! country US: DFS-FCC
//!   (2402 - 2472 @ 40), (N/A, 30), (N/A)
//!   ...
//!
//! phy#1 (self-managed)
//! country IN: DFS-UNSET
//!   ...
//! ```
//!
//! The reader is subprocess-backed and runs on the async side like the Pi
//! throttle reader: a bounded timeout with a kill on overrun so a hung `iw`
//! never accumulates, and a graceful skip (no signals for the tick) when `iw` is
//! absent, errors, times out, or prints nothing parseable. It never touches an
//! interface; `iw reg get` is a read-only query.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use rmpv::Value as MpVal;
use tokio::process::Command;
use tokio::time::timeout;

/// How long `iw reg get` is allowed to run before it is killed. The call is
/// cheap; the bound guards against a hung netlink query.
pub const IW_REG_TIMEOUT: Duration = Duration::from_secs(2);

/// One regulatory block parsed from `iw reg get`: the two-character country code
/// and the optional DFS region tag (`FCC`, `ETSI`, `UNSET`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegBlock {
    /// `None` for the global block, `Some(n)` for a self-managed `phy#n` block.
    pub phy: Option<u32>,
    /// ISO 3166-1 alpha-2 country code, uppercased (`US`, `IN`, `00`).
    pub country: String,
    /// The DFS region tag after the country, when present (`FCC`, `ETSI`, ...).
    pub dfs_region: Option<String>,
}

/// Parse `iw reg get` output into its regulatory blocks (global + per
/// self-managed phy). Pure so the parse is unit-testable without `iw`.
///
/// A `phy#N (self-managed)` header (tolerant of the `self managed` /
/// `self-managed` spelling variants `iw` has used) opens a per-phy block; the
/// first `country XX:` line after it belongs to that phy. A `country` line seen
/// before any phy header is the global domain. The `DFS-REGION` tag after the
/// country (e.g. `country US: DFS-FCC`) is captured when present.
pub fn parse_iw_reg_get(text: &str) -> Vec<RegBlock> {
    let mut out = Vec::new();
    // The phy a subsequent `country` line is attributed to: `None` until a
    // self-managed phy header is seen, so the first country is the global one.
    let mut pending_phy: Option<u32> = None;
    for line in text.lines() {
        let s = line.trim();
        let low = s.to_ascii_lowercase();
        if low.starts_with("phy") && (low.contains("self managed") || low.contains("self-managed"))
        {
            let raw = s.split_whitespace().next().unwrap_or("");
            let digits: String = raw
                .trim_start_matches("phy#")
                .trim_start_matches("phy")
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            pending_phy = digits.parse::<u32>().ok();
            continue;
        }
        if let Some(rest) = s.strip_prefix("country ") {
            let country: String = rest
                .chars()
                .take(2)
                .collect::<String>()
                .to_ascii_uppercase();
            // A real code is two alphanumerics (`US`, `IN`, `00`); reject a
            // truncated token like `X:` whose second char is punctuation.
            if country.len() != 2 || !country.bytes().all(|b| b.is_ascii_alphanumeric()) {
                continue;
            }
            let dfs_region = parse_dfs_region(rest);
            out.push(RegBlock {
                phy: pending_phy.take(),
                country,
                dfs_region,
            });
        }
    }
    out
}

/// Pull the DFS region tag out of the tail of a `country XX: DFS-REGION` line.
/// Returns e.g. `Some("FCC")` for `US: DFS-FCC`, `None` when no `DFS-` tag is
/// present. The tag is uppercased and trimmed to the alphanumeric region token.
fn parse_dfs_region(rest: &str) -> Option<String> {
    let upper = rest.to_ascii_uppercase();
    let idx = upper.find("DFS-")?;
    let tag: String = upper[idx + 4..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (!tag.is_empty()).then_some(tag)
}

/// Read the active regulatory domain via `iw reg get` and fold it into the
/// dotted-signal map the HW snapshot carries. Keys:
///
/// - `reg.global.country` / `reg.global.dfs_region` — the global domain;
/// - `reg.phy<N>.country` / `reg.phy<N>.dfs_region` — each self-managed phy.
///
/// A block with no DFS tag contributes only the `*.country` key. Returns an
/// empty map (graceful skip) when `iw` is absent / errors / times out / prints
/// nothing parseable, so a board without `iw` never aborts a tick.
pub async fn read_regdomain() -> BTreeMap<String, MpVal> {
    let mut signals = BTreeMap::new();
    let Some(stdout) = run_iw_reg_get().await else {
        return signals;
    };
    for block in parse_iw_reg_get(&stdout) {
        let prefix = match block.phy {
            None => "reg.global".to_string(),
            Some(n) => format!("reg.phy{n}"),
        };
        signals.insert(format!("{prefix}.country"), MpVal::from(block.country));
        if let Some(region) = block.dfs_region {
            signals.insert(format!("{prefix}.dfs_region"), MpVal::from(region));
        }
    }
    signals
}

/// Spawn `iw reg get` with a bounded timeout and a kill on overrun, returning its
/// stdout. Any spawn / exit / timeout failure is a graceful `None`.
async fn run_iw_reg_get() -> Option<String> {
    let child = Command::new("iw")
        .args(["reg", "get"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;

    let wait = child.wait_with_output();
    match timeout(IW_REG_TIMEOUT, wait).await {
        Ok(Ok(output)) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        // Non-zero exit, spawn-side IO error, or the timeout elapsed (the child
        // is killed on drop): record no regulatory signals for this tick.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_global_only_domain() {
        let blocks = parse_iw_reg_get("global\ncountry US: DFS-FCC\n  (2402 - 2472 @ 40)\n");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].phy, None);
        assert_eq!(blocks[0].country, "US");
        assert_eq!(blocks[0].dfs_region.as_deref(), Some("FCC"));
    }

    #[test]
    fn parses_global_plus_self_managed_phy() {
        let text = "\
global
country US: DFS-FCC
  (5170 - 5250 @ 80), (N/A, 17), (N/A)

phy#1 (self-managed)
country IN: DFS-ETSI
  (2402 - 2482 @ 40), (6, 20), (N/A)
";
        let blocks = parse_iw_reg_get(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].phy, None);
        assert_eq!(blocks[0].country, "US");
        assert_eq!(blocks[0].dfs_region.as_deref(), Some("FCC"));
        assert_eq!(blocks[1].phy, Some(1));
        assert_eq!(blocks[1].country, "IN");
        assert_eq!(blocks[1].dfs_region.as_deref(), Some("ETSI"));
    }

    #[test]
    fn handles_self_managed_spelling_variant_and_no_dfs_tag() {
        // The `self managed` (space) spelling and a country with no DFS- tag.
        let text = "\
country 00:
  (2402 - 2472 @ 40)

phy#0 (self managed)
country DE:
";
        let blocks = parse_iw_reg_get(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].phy, None);
        assert_eq!(blocks[0].country, "00");
        assert_eq!(blocks[0].dfs_region, None);
        assert_eq!(blocks[1].phy, Some(0));
        assert_eq!(blocks[1].country, "DE");
        assert_eq!(blocks[1].dfs_region, None);
    }

    #[test]
    fn read_regdomain_folds_blocks_into_dotted_keys() {
        // Exercise the fold logic directly with the parser output so the key
        // shape is asserted without spawning `iw`.
        let text = "country US: DFS-FCC\n\nphy#2 (self-managed)\ncountry IN: DFS-ETSI\n";
        let mut signals = BTreeMap::new();
        for block in parse_iw_reg_get(text) {
            let prefix = match block.phy {
                None => "reg.global".to_string(),
                Some(n) => format!("reg.phy{n}"),
            };
            signals.insert(format!("{prefix}.country"), MpVal::from(block.country));
            if let Some(region) = block.dfs_region {
                signals.insert(format!("{prefix}.dfs_region"), MpVal::from(region));
            }
        }
        assert_eq!(
            signals.get("reg.global.country").and_then(|v| v.as_str()),
            Some("US")
        );
        assert_eq!(
            signals
                .get("reg.global.dfs_region")
                .and_then(|v| v.as_str()),
            Some("FCC")
        );
        assert_eq!(
            signals.get("reg.phy2.country").and_then(|v| v.as_str()),
            Some("IN")
        );
        assert_eq!(
            signals.get("reg.phy2.dfs_region").and_then(|v| v.as_str()),
            Some("ETSI")
        );
    }

    #[test]
    fn empty_or_garbage_output_yields_no_blocks() {
        assert!(parse_iw_reg_get("").is_empty());
        assert!(parse_iw_reg_get("nonsense without a country line").is_empty());
        // A `country` token with a one-char code is rejected.
        assert!(parse_iw_reg_get("country X:\n").is_empty());
    }

    #[tokio::test]
    async fn read_regdomain_is_graceful_when_iw_is_absent() {
        // On CI / a dev host without `iw` (or where it errors), the read returns
        // an empty map rather than panicking or aborting the tick. This is a
        // smoke test of the graceful-skip path; it does not assert a domain.
        let _signals = read_regdomain().await;
    }
}
