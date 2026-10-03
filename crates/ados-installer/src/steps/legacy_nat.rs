//! Legacy NAT cleanup: remove uplink-sharing NAT state that lives outside the
//! ADOS-owned places.
//!
//! The firewall manager keeps its MASQUERADE rule in a dedicated chain
//! (`ADOS_NAT` for iptables) or in the `ados_nat` table persisted through the
//! `/etc/nftables.d/ados-nat.nft` include. A node can still carry an older
//! form of the same intent:
//!
//!   - a bare `-A POSTROUTING -o <iface> -j MASQUERADE` rule in the live nat
//!     table and in `/etc/iptables/rules.v4`, which would keep masquerading a
//!     previous uplink no matter what the firewall manager decides;
//!   - a `table ip ados_nat { ... }` block written straight into
//!     `/etc/nftables.conf`, which fights the include on every boot, with no
//!     `include "/etc/nftables.d/*.nft"` line to load the include at all.
//!
//! This step removes those forms and adds the include line. It runs on every
//! install and upgrade (no checkpoint): every effect is idempotent, so a clean
//! node is a no-op. The `-A POSTROUTING -j ADOS_NAT` jump and the live
//! `ados_nat` table belong to the firewall manager and are never touched.
//!
//! Optional: a failed command or write degrades, never aborts the install.

use std::path::Path;

use crate::ctx::Ctx;
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};

/// iptables-persistent's saved IPv4 ruleset.
const RULES_V4_PATH: &str = "/etc/iptables/rules.v4";
/// The system nftables configuration loaded at boot.
const NFTABLES_CONF_PATH: &str = "/etc/nftables.conf";
/// The include line that loads the ADOS-owned nftables drop-ins.
const NFT_INCLUDE_LINE: &str = "include \"/etc/nftables.d/*.nft\"";
/// Substring that marks an existing nftables.d include.
const NFT_INCLUDE_DIR: &str = "/etc/nftables.d/";
/// Opening of the ADOS NAT table when declared inline in a conf file.
const NAT_TABLE_HEAD: &str = "table ip ados_nat";
/// Upper bound on delete passes per interface, so a `-D` that reports success
/// without removing anything cannot spin forever.
const MAX_DELETES_PER_IFACE: usize = 64;

/// The interface of a bare uplink MASQUERADE rule, when `line` is exactly
/// `-A POSTROUTING -o <iface> -j MASQUERADE` (trailing whitespace and CR
/// ignored). Any other match, source or extra option means the rule is not
/// the legacy form and is left alone.
fn legacy_masquerade_iface(line: &str) -> Option<&str> {
    let iface = line
        .trim_end()
        .strip_prefix("-A POSTROUTING -o ")?
        .strip_suffix(" -j MASQUERADE")?;
    (!iface.is_empty() && !iface.contains(char::is_whitespace)).then_some(iface)
}

/// The interfaces named by bare uplink MASQUERADE rules in `iptables -S`
/// (or `iptables-save`) output, in first-seen order without duplicates. Pure.
pub fn legacy_masquerade_ifaces(iptables_s: &str) -> Vec<String> {
    let mut ifaces: Vec<String> = Vec::new();
    for iface in iptables_s.lines().filter_map(legacy_masquerade_iface) {
        if !ifaces.iter().any(|known| known == iface) {
            ifaces.push(iface.to_string());
        }
    }
    ifaces
}

/// `rules_v4` without its bare uplink MASQUERADE lines, or `None` when it has
/// none. Every other line, and the file's line endings, stay as they were.
/// Pure.
pub fn strip_legacy_masquerade(rules_v4: &str) -> Option<String> {
    let mut out = String::with_capacity(rules_v4.len());
    let mut removed = false;
    for line in rules_v4.split_inclusive('\n') {
        if legacy_masquerade_iface(line).is_some() {
            removed = true;
        } else {
            out.push_str(line);
        }
    }
    removed.then_some(out)
}

/// Byte range of the first inline `table ip ados_nat { ... }` block: from the
/// start of its line through the matching closing brace, plus the rest of
/// that line when only whitespace follows the brace. Braces inside `#`
/// comments and double-quoted strings do not count. `None` when there is no
/// such block or its braces never balance (a file this step cannot parse is
/// left alone).
fn ados_nat_block(conf: &str) -> Option<(usize, usize)> {
    let mut line_start = 0;
    for line in conf.split_inclusive('\n') {
        let indent = line.len() - line.trim_start().len();
        let head = line[indent..]
            .strip_prefix(NAT_TABLE_HEAD)
            .and_then(|rest| {
                let after = rest.trim_start_matches([' ', '\t']);
                after.starts_with('{').then(|| rest.len() - after.len())
            });
        if let Some(gap) = head {
            let open = line_start + indent + NAT_TABLE_HEAD.len() + gap;
            let close = matching_brace(conf, open)?;
            let tail = &conf[close + 1..];
            let line_rest = tail.find('\n').map_or(tail.len(), |i| i + 1);
            let end = if tail[..line_rest].trim().is_empty() {
                close + 1 + line_rest
            } else {
                close + 1
            };
            return Some((line_start, end));
        }
        line_start += line.len();
    }
    None
}

/// Index of the `}` that closes the `{` at `open`.
fn matching_brace(conf: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_comment = false;
    let mut in_string = false;
    for (i, c) in conf[open..].char_indices() {
        match c {
            '\n' => in_comment = false,
            _ if in_comment => {}
            '"' => in_string = !in_string,
            _ if in_string => {}
            '#' => in_comment = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `nft_conf` without any inline `table ip ados_nat { ... }` block, or `None`
/// when it has none. Pure.
pub fn strip_ados_nat_table(nft_conf: &str) -> Option<String> {
    let mut conf = nft_conf.to_string();
    let mut removed = false;
    while let Some((start, end)) = ados_nat_block(&conf) {
        conf.replace_range(start..end, "");
        removed = true;
    }
    removed.then_some(conf)
}

/// `nft_conf` with `include "/etc/nftables.d/*.nft"` appended on its own line,
/// or `None` when an uncommented line already includes `/etc/nftables.d/`.
/// Pure.
pub fn ensure_nft_include(nft_conf: &str) -> Option<String> {
    let present = nft_conf.lines().any(|line| {
        let line = line.trim_start();
        !line.starts_with('#') && line.contains(NFT_INCLUDE_DIR)
    });
    if present {
        return None;
    }
    let mut conf = String::with_capacity(nft_conf.len() + NFT_INCLUDE_LINE.len() + 2);
    conf.push_str(nft_conf);
    if !conf.is_empty() && !conf.ends_with('\n') {
        conf.push('\n');
    }
    conf.push_str(NFT_INCLUDE_LINE);
    conf.push('\n');
    Some(conf)
}

/// Both nftables.conf transforms in order, or `None` when neither changes it.
fn clean_nft_conf(nft_conf: &str) -> Option<String> {
    let stripped = strip_ados_nat_table(nft_conf);
    ensure_nft_include(stripped.as_deref().unwrap_or(nft_conf)).or(stripped)
}

/// Apply `transform` to the file at `path` and write the result durably when
/// it changes. A missing file is not an error. Returns whether it wrote.
fn rewrite_if_changed(
    path: &Path,
    transform: impl Fn(&str) -> Option<String>,
) -> Result<bool, String> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let Some(updated) = transform(&body) else {
        return Ok(false);
    };
    ados_protocol::sidecar::write_durable(path, updated.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    tracing::info!(path = %path.display(), "removed legacy NAT configuration from file");
    Ok(true)
}

/// True when `iptables` resolves on PATH or in the sbin directories root's
/// PATH normally carries.
fn iptables_present() -> bool {
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join("iptables").is_file()));
    on_path
        || ["/usr/sbin/iptables", "/sbin/iptables"]
            .iter()
            .any(|p| Path::new(p).is_file())
}

/// Delete every bare uplink MASQUERADE rule from the live nat table.
fn remove_live_rules() -> Result<(), String> {
    if !iptables_present() {
        return Ok(());
    }
    let listing = exec::run("iptables", &["-t", "nat", "-S", "POSTROUTING"]);
    if !listing.success() {
        return Err(format!(
            "iptables -t nat -S POSTROUTING failed: {}",
            listing.stderr.trim()
        ));
    }
    for iface in legacy_masquerade_ifaces(&listing.stdout) {
        let rule = ["POSTROUTING", "-o", iface.as_str(), "-j", "MASQUERADE"];
        let with = |op: &'static str| -> Vec<&str> {
            let mut argv = vec!["-t", "nat", op];
            argv.extend_from_slice(&rule);
            argv
        };
        for _ in 0..MAX_DELETES_PER_IFACE {
            if !exec::run_ok("iptables", &with("-C")) {
                break;
            }
            let del = exec::run("iptables", &with("-D"));
            if !del.success() {
                return Err(format!(
                    "iptables delete of the MASQUERADE rule on {iface} failed: {}",
                    del.stderr.trim()
                ));
            }
            tracing::info!(iface = %iface, "removed legacy uplink MASQUERADE rule");
        }
    }
    Ok(())
}

/// Idempotent removal of legacy uplink NAT rules and inline NAT tables.
pub struct LegacyNat;

impl Step for LegacyNat {
    fn id(&self) -> &str {
        "legacy_nat"
    }
    fn requires(&self) -> &[&str] {
        &[]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: every effect is idempotent, so it re-runs on each
        // install and upgrade and is a no-op on a clean node.
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Optional
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        let errors: Vec<String> = [
            remove_live_rules(),
            rewrite_if_changed(Path::new(RULES_V4_PATH), strip_legacy_masquerade).map(drop),
            rewrite_if_changed(Path::new(NFTABLES_CONF_PATH), clean_nft_conf).map(drop),
        ]
        .into_iter()
        .filter_map(Result::err)
        .collect();
        if errors.is_empty() {
            StepOutcome::Ok
        } else {
            StepOutcome::Failed(errors.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES_V4: &str = "*nat\n\
        :PREROUTING ACCEPT [0:0]\n\
        :POSTROUTING ACCEPT [0:0]\n\
        :ADOS_NAT - [0:0]\n\
        -A POSTROUTING -o wwan0 -j MASQUERADE\n\
        -A POSTROUTING -j ADOS_NAT\n\
        -A POSTROUTING -s 10.0.0.0/8 -j ACCEPT\n\
        -A ADOS_NAT -o eth0 -j MASQUERADE\n\
        COMMIT\n";

    const NFT_CONF: &str = "#!/usr/sbin/nft -f\n\
        \n\
        flush ruleset\n\
        \n\
        table ip ados_nat {\n\
        \tchain postrouting {\n\
        \t\ttype nat hook postrouting priority srcnat; policy accept;\n\
        \t\toifname \"wwan0\" masquerade # uplink {legacy\n\
        \t}\n\
        }\n\
        table inet filter {\n\
        \tchain input {\n\
        \t\ttype filter hook input priority filter;\n\
        \t}\n\
        }\n";

    #[test]
    fn only_the_exact_bare_masquerade_form_names_an_interface() {
        let listing = "-P POSTROUTING ACCEPT\n\
            -A POSTROUTING -o wwan0 -j MASQUERADE\r\n\
            -A POSTROUTING -o usb0 -j MASQUERADE  \n\
            -A POSTROUTING -o wwan0 -j MASQUERADE\n\
            -A POSTROUTING -j ADOS_NAT\n\
            -A POSTROUTING -s 10.0.0.0/8 -o eth0 -j MASQUERADE\n\
            -A POSTROUTING -o eth0 -j MASQUERADE --random\n\
            -A ADOS_NAT -o eth0 -j MASQUERADE\n";
        assert_eq!(legacy_masquerade_ifaces(listing), vec!["wwan0", "usb0"]);
        assert!(legacy_masquerade_ifaces("-A POSTROUTING -o  -j MASQUERADE\n").is_empty());
    }

    #[test]
    fn rules_v4_loses_only_the_bare_uplink_masquerade() {
        let stripped = strip_legacy_masquerade(RULES_V4).expect("the legacy rule is present");
        assert_eq!(
            stripped,
            RULES_V4.replace("-A POSTROUTING -o wwan0 -j MASQUERADE\n", "")
        );
        assert!(stripped.contains("-A POSTROUTING -j ADOS_NAT\n"));
        assert!(stripped.contains("-A POSTROUTING -s 10.0.0.0/8 -j ACCEPT\n"));
        assert!(stripped.contains("-A ADOS_NAT -o eth0 -j MASQUERADE\n"));
        assert!(stripped.ends_with("COMMIT\n"));
    }

    #[test]
    fn nftables_conf_loses_only_the_ados_nat_table_and_gains_one_include() {
        let cleaned = clean_nft_conf(NFT_CONF).expect("the conf needs cleaning");
        assert!(!cleaned.contains("ados_nat"));
        assert!(!cleaned.contains("masquerade"));
        assert!(cleaned.starts_with("#!/usr/sbin/nft -f\n\nflush ruleset\n\ntable inet filter {\n"));
        assert!(cleaned.contains("\t\ttype filter hook input priority filter;\n\t}\n}\n"));
        assert_eq!(cleaned.matches(NFT_INCLUDE_LINE).count(), 1);
        assert!(cleaned.ends_with(&format!("}}\n{NFT_INCLUDE_LINE}\n")));
    }

    #[test]
    fn a_one_line_ados_nat_table_is_removed_and_a_lookalike_name_is_kept() {
        let conf = "table ip ados_nat { chain postrouting { type nat hook postrouting priority srcnat; } }\n\
            table ip ados_nat2 { }\n";
        assert_eq!(
            strip_ados_nat_table(conf).as_deref(),
            Some("table ip ados_nat2 { }\n")
        );
    }

    #[test]
    fn an_unbalanced_ados_nat_table_is_left_alone() {
        assert!(strip_ados_nat_table("table ip ados_nat {\n\tchain postrouting {\n}\n").is_none());
    }

    #[test]
    fn the_include_is_appended_on_its_own_line_and_never_twice() {
        assert_eq!(
            ensure_nft_include("flush ruleset").as_deref(),
            Some("flush ruleset\ninclude \"/etc/nftables.d/*.nft\"\n")
        );
        assert!(ensure_nft_include("include \"/etc/nftables.d/ados-nat.nft\"\n").is_none());
        // A commented-out include loads nothing, so it does not count.
        assert!(ensure_nft_include("# include \"/etc/nftables.d/*.nft\"\n").is_some());
    }

    #[test]
    fn a_second_pass_changes_nothing() {
        let rules = strip_legacy_masquerade(RULES_V4).unwrap();
        assert!(strip_legacy_masquerade(&rules).is_none());
        assert!(legacy_masquerade_ifaces(&rules).is_empty());

        let conf = clean_nft_conf(NFT_CONF).unwrap();
        assert!(strip_ados_nat_table(&conf).is_none());
        assert!(ensure_nft_include(&conf).is_none());
        assert!(clean_nft_conf(&conf).is_none());
    }

    #[test]
    fn a_file_is_rewritten_once_and_a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.v4");
        assert_eq!(
            rewrite_if_changed(&path, strip_legacy_masquerade),
            Ok(false)
        );

        std::fs::write(&path, RULES_V4).unwrap();
        assert_eq!(rewrite_if_changed(&path, strip_legacy_masquerade), Ok(true));
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("-o wwan0 -j MASQUERADE"));
        assert_eq!(
            rewrite_if_changed(&path, strip_legacy_masquerade),
            Ok(false)
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
