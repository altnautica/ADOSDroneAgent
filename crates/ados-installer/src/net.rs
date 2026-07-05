//! IPv4-resilient artifact fetcher.
//!
//! Shells out to `curl` (the workspace `ureq` is HTTP-only / no-TLS and cannot
//! fetch GitHub release assets over HTTPS). The fetch must survive the
//! confirmed field failure where a host resolves an asset to IPv6-only with no
//! usable IPv6 default route and a naive `curl` stalls ~12 s before failing:
//! force `-4` when no IPv6 default route exists, and retry with `-4` on a
//! connect timeout. Bounded retries; never hangs (curl `--max-time`).

use std::path::Path;
use std::time::Duration;

use crate::exec;

/// Build the `curl` argument vector for a single fetch attempt.
///
/// Pure (no I/O): silent + fail-on-error + follow-redirects (`-fsSL`), a 10 s
/// connect timeout, a 180 s overall ceiling so a stalled transfer can never
/// hang the install, and three bounded retries with a 2 s delay. `--continue-at
/// -` resumes a partially-downloaded file: on a flaky link a multi-MB asset that
/// drops mid-transfer continues from the last byte on the next of curl's
/// `--retry` attempts (GitHub release assets support range requests) instead of
/// restarting from zero. When `force_ipv4` is set, `-4` is inserted so curl
/// skips a dead IPv6 default route instead of stalling on it. The destination is
/// the caller's path verbatim (the atomic temp-then-rename dance is `fetch`'s
/// job, not this builder's).
///
/// Cache-busting: rolling tags reuse one asset URL across rebuilds, so an
/// intermediary CDN can hand back a stale binary paired with a stale sha. The
/// `Cache-Control: no-cache` + `Pragma: no-cache` request headers force a
/// revalidation so the binary and its sha256 are always the freshly published
/// pair.
pub fn curl_args(url: &str, dest: &Path, force_ipv4: bool) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-fsSL".to_string(),
        "-H".to_string(),
        "Cache-Control: no-cache".to_string(),
        "-H".to_string(),
        "Pragma: no-cache".to_string(),
        "--connect-timeout".to_string(),
        "10".to_string(),
        "--max-time".to_string(),
        "180".to_string(),
        "--retry".to_string(),
        "3".to_string(),
        "--retry-delay".to_string(),
        "2".to_string(),
        // Resume a partial transfer across curl's own --retry attempts so a
        // mid-download drop on a flaky link continues instead of restarting.
        "--continue-at".to_string(),
        "-".to_string(),
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
    match download_to_tmp(url, &tmp) {
        Ok(()) => finalize(&tmp, dest),
        Err(e) => {
            // Clean up the partial temp file on the way out.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Fetch `url` into `dest` like [`fetch`], reporting byte progress via
/// `on_bytes(downloaded, total)` as the transfer runs. `total` is the HTTP
/// `Content-Length` (0 when the server does not advertise one — the caller then
/// shows an indeterminate size). The download runs on a background thread while
/// this thread polls the temp file's on-disk length; `on_bytes` runs on the
/// caller thread (no `Send` bound). Used by the component-download step so the
/// live pane shows "ados-control 4.2/8.1 MB" instead of a bare spinner.
pub fn fetch_with_progress<F: FnMut(u64, u64)>(
    url: &str,
    dest: &Path,
    mut on_bytes: F,
) -> anyhow::Result<()> {
    let total = head_content_length(url).unwrap_or(0);
    let tmp = tmp_sibling(dest);
    // A stale partial from a prior run would make `--continue-at -` resume the
    // wrong bytes; start clean.
    let _ = std::fs::remove_file(&tmp);

    let url_owned = url.to_string();
    let tmp_dl = tmp.clone();
    let handle = std::thread::spawn(move || download_to_tmp(&url_owned, &tmp_dl));

    while !handle.is_finished() {
        if let Ok(m) = std::fs::metadata(&tmp) {
            on_bytes(m.len(), total);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Final size (the poll may have missed the last chunk before completion).
    if let Ok(m) = std::fs::metadata(&tmp) {
        on_bytes(m.len(), total);
    }

    let result = handle
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("download thread panicked")));
    match result {
        Ok(()) => finalize(&tmp, dest),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Download `url` to `tmp` (no rename), IPv4-resilient with the same dual-stack
/// → `-4` fallback [`fetch`] uses. Shared by [`fetch`] and
/// [`fetch_with_progress`].
fn download_to_tmp(url: &str, tmp: &Path) -> anyhow::Result<()> {
    let force_first = !has_ipv6_default_route();
    match curl_to(url, tmp, force_first) {
        Ok(()) => Ok(()),
        Err(_) if !force_first => {
            // Dual-stack attempt failed and we have not yet forced v4 — retry
            // with `-4` (present-but-broken IPv6 default route).
            tracing::warn!(url, "fetch failed on dual-stack; retrying with -4");
            curl_to(url, tmp, true)
        }
        Err(e) => Err(e),
    }
}

/// Atomically move the completed temp download to its destination.
fn finalize(tmp: &Path, dest: &Path) -> anyhow::Result<()> {
    std::fs::rename(tmp, dest).map_err(|e| {
        let _ = std::fs::remove_file(tmp);
        anyhow::anyhow!("rename {} -> {} failed: {e}", tmp.display(), dest.display())
    })
}

/// Best-effort `Content-Length` for `url` via a `curl -sIL` HEAD (follows
/// redirects). `None` when the HEAD fails or advertises no length; the caller
/// treats that as an unknown total.
fn head_content_length(url: &str) -> Option<u64> {
    let mut args: Vec<&str> = vec!["-sIL", "--max-time", "15"];
    if !has_ipv6_default_route() {
        args.push("-4");
    }
    args.push(url);
    let res = exec::run("curl", &args);
    if !res.success() {
        return None;
    }
    parse_content_length(&res.stdout)
}

/// Parse the `Content-Length` from a block of HTTP response headers (pure).
/// Takes the LAST occurrence so a redirect chain's final (real) response wins
/// over an intermediate 302's length. Case-insensitive on the header name.
pub fn parse_content_length(headers: &str) -> Option<u64> {
    headers
        .lines()
        .filter_map(|l| {
            let (k, v) = l.trim().split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse::<u64>().ok()
            } else {
                None
            }
        })
        .next_back()
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
        // The 180 s ceiling value follows --max-time.
        let pos = args.iter().position(|a| a == "--max-time").unwrap();
        assert_eq!(args[pos + 1], "180");
        assert!(!args.contains(&"-4".to_string()));
        // Resume is enabled so a dropped transfer continues from the last byte.
        let cpos = args.iter().position(|a| a == "--continue-at").unwrap();
        assert_eq!(args[cpos + 1], "-");
        // Cache-busting headers are present so a CDN cannot serve a stale pair.
        assert!(args.contains(&"Cache-Control: no-cache".to_string()));
        assert!(args.contains(&"Pragma: no-cache".to_string()));
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

    #[test]
    fn parse_content_length_takes_the_final_hop() {
        // A redirect chain: the 302 has no body length, the final 200 does.
        let headers = "HTTP/2 302\r\nlocation: https://cdn/x\r\ncontent-length: 0\r\n\r\n\
                       HTTP/2 200\r\nContent-Length: 8523776\r\ncontent-type: application/octet-stream\r\n";
        assert_eq!(parse_content_length(headers), Some(8_523_776));
    }

    #[test]
    fn parse_content_length_none_when_absent() {
        assert_eq!(
            parse_content_length("HTTP/2 200\r\ncontent-type: x\r\n"),
            None
        );
    }
}
