//! IPv4-resilient artifact fetcher.
//!
//! Shells out to `curl` (the workspace `ureq` is HTTP-only / no-TLS and cannot
//! fetch GitHub release assets over HTTPS). The fetch must survive the
//! confirmed field failure where a host resolves an asset to IPv6-only with no
//! usable IPv6 default route and a naive `curl` stalls ~12 s before failing:
//! force `-4` when no IPv6 default route exists, and retry with `-4` on a
//! connect timeout. Bounded retries; never hangs (curl `--max-time`).

use std::path::Path;

use crate::exec;

/// Build the `curl` argument vector for a single fetch attempt.
///
/// Pure (no I/O): the same flag set the predecessor bash `ados_fetch` used —
/// silent + fail-on-error + follow-redirects (`-fsSL`), a 10 s connect timeout,
/// a 120 s overall ceiling so a stalled transfer can never hang the install,
/// and three bounded retries with a 2 s delay. When `force_ipv4` is set, `-4`
/// is inserted so curl skips a dead IPv6 default route instead of stalling on
/// it. The destination is the caller's path verbatim (the atomic temp-then-
/// rename dance is `fetch`'s job, not this builder's).
pub fn curl_args(url: &str, dest: &Path, force_ipv4: bool) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-fsSL".to_string(),
        "--connect-timeout".to_string(),
        "10".to_string(),
        "--max-time".to_string(),
        "120".to_string(),
        "--retry".to_string(),
        "3".to_string(),
        "--retry-delay".to_string(),
        "2".to_string(),
    ];
    if force_ipv4 {
        args.push("-4".to_string());
    }
    args.push(url.to_string());
    args.push("-o".to_string());
    args.push(dest.to_string_lossy().into_owned());
    args
}

/// True when the host has a usable IPv6 default route. We only force `-4` up
/// front when this is false (the field-failure case: AAAA records resolve but
/// there is no v6 path off the box). Probes `ip -6 route show default`; treats
/// a missing `ip` binary or any error as "no v6 default route" (force `-4`).
pub fn has_ipv6_default_route() -> bool {
    let res = exec::run("ip", &["-6", "route", "show", "default"]);
    res.success() && res.stdout.lines().any(|l| l.trim().starts_with("default"))
}

/// Run one `curl` fetch attempt to `dest`. Returns `Ok(())` on a clean exit 0.
fn curl_to(url: &str, dest: &Path, force_ipv4: bool) -> anyhow::Result<()> {
    let args = curl_args(url, dest, force_ipv4);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = exec::run("curl", &argv);
    if res.success() {
        return Ok(());
    }
    if !res.spawned {
        anyhow::bail!("`curl` is not installed: {}", res.stderr.trim());
    }
    anyhow::bail!(
        "curl exited {:?} fetching {url}: {}",
        res.code,
        res.stderr.trim()
    );
}

/// Fetch `url` into `dest` (atomically: download to a temp sibling then rename),
/// IPv4-resilient with bounded retries. Returns `Err` only after retries are
/// exhausted.
///
/// Order of attempts:
///   1. If the host has no IPv6 default route, go straight to `-4` (the field
///      failure is an AAAA record with no v6 path; the dual-stack attempt would
///      just burn the connect timeout first).
///   2. Otherwise try a normal (dual-stack) fetch, and on failure retry once
///      with `-4` in case the v6 path is present-but-broken.
pub fn fetch(url: &str, dest: &Path) -> anyhow::Result<()> {
    // Download to a temp sibling so a partial/failed transfer never leaves a
    // truncated file at the real destination (a later verify would mis-read it).
    let tmp = tmp_sibling(dest);

    let force_first = !has_ipv6_default_route();
    let first = curl_to(url, &tmp, force_first);
    let result = match first {
        Ok(()) => Ok(()),
        Err(_) if !force_first => {
            // Dual-stack attempt failed and we have not yet forced v4 — retry
            // with `-4` (present-but-broken IPv6 default route).
            tracing::warn!(url, "fetch failed on dual-stack; retrying with -4");
            curl_to(url, &tmp, true)
        }
        Err(e) => Err(e),
    };

    match result {
        Ok(()) => {
            std::fs::rename(&tmp, dest).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                anyhow::anyhow!("rename {} -> {} failed: {e}", tmp.display(), dest.display())
            })?;
            Ok(())
        }
        Err(e) => {
            // Clean up the partial temp file on the way out.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// `<dest>.tmp` sibling path used for the atomic download.
fn tmp_sibling(dest: &Path) -> std::path::PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".tmp");
    std::path::PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn curl_args_has_max_time_and_no_v4_by_default() {
        let args = curl_args("https://example/x", Path::new("/tmp/x"), false);
        assert!(args.contains(&"--max-time".to_string()));
        // The 120 s ceiling value follows --max-time.
        let pos = args.iter().position(|a| a == "--max-time").unwrap();
        assert_eq!(args[pos + 1], "120");
        assert!(!args.contains(&"-4".to_string()));
        // -fsSL is first, URL precedes -o <dest>.
        assert_eq!(args[0], "-fsSL");
        let url_pos = args.iter().position(|a| a == "https://example/x").unwrap();
        assert_eq!(args[url_pos + 1], "-o");
        assert_eq!(args[url_pos + 2], "/tmp/x");
    }

    #[test]
    fn curl_args_adds_v4_flag_when_forced() {
        let args = curl_args("https://example/x", Path::new("/tmp/x"), true);
        assert!(args.contains(&"-4".to_string()));
        // The connect timeout is always present regardless of -4.
        assert!(args.contains(&"--connect-timeout".to_string()));
    }

    #[test]
    fn tmp_sibling_appends_tmp() {
        let t = tmp_sibling(Path::new("/opt/ados/bin/ados-video"));
        assert_eq!(t.to_str().unwrap(), "/opt/ados/bin/ados-video.tmp");
    }
}
