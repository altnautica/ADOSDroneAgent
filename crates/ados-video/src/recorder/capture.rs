//! Capture-segment spawn and supervision, plus the filesystem and time helpers.

use std::path::Path;

use super::*;

/// Spawn one capture segment into `output_path`.
///
/// Reads the local RTSP source over TCP and remuxes to MP4 with `-c copy` (no
/// transcode). The muxer flags are the crash-safety contract, not tuning. A
/// plain (or `+faststart`) MP4 writes its `moov` atom only on a clean exit, so
/// a ground station that loses power, is SIGKILLed or fills its disk
/// mid-capture leaves a file with no index at all — not a truncated
/// recording, a zero-recoverable one.
///
/// `empty_moov` puts a valid (empty) index at byte 0 the moment the file is
/// created; `frag_keyframe` closes a self-describing `moof`+`mdat` fragment at
/// every IDR, so with the shipped 0.5 s GOP at most half a second of video is
/// ever un-indexed; `default_base_moof` makes each fragment's offsets relative
/// to its own `moof`. `-flush_packets 1` pushes each packet out of ffmpeg's
/// AVIO buffer, which bounds the loss to the current GOP.
///
/// Spawned as a process-group leader so the whole pipeline is reaped on stop.
/// ffmpeg's stderr is drained: a pipe nobody reads fills at 64 KB and blocks
/// the muxer's next write.
pub(super) fn spawn_capture(
    ffmpeg: &str,
    rtsp_url: &str,
    output_path: &Path,
) -> std::io::Result<ManagedProcess> {
    let args: Vec<String> = vec![
        "-y".to_string(),
        "-rtsp_transport".to_string(),
        "tcp".to_string(),
        "-i".to_string(),
        rtsp_url.to_string(),
        "-c".to_string(),
        "copy".to_string(),
        "-movflags".to_string(),
        RECORDER_MOVFLAGS.to_string(),
        "-flush_packets".to_string(),
        "1".to_string(),
        output_path.to_string_lossy().to_string(),
    ];
    let mut process = ManagedProcess::spawn("gs-recorder", ffmpeg, &args)?;
    if let Some(stderr) = process.take_stderr() {
        tokio::spawn(crate::stderr_drain::drain_plain(stderr, "gs-recorder"));
    }
    Ok(process)
}

/// `<stem>_part<n>.mp4` beside `first`.
pub(super) fn segment_path(first: &Path, n: usize) -> PathBuf {
    let stem = first
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("recording");
    first.with_file_name(format!("{stem}_part{n}.mp4"))
}

/// Keep a wanted capture running: every [`SUPERVISE_INTERVAL`], if the ffmpeg
/// child has exited (or a previous respawn failed), start the next segment.
pub(super) async fn supervise(
    inner: Arc<Mutex<Inner>>,
    ffmpeg: String,
    rtsp_url: String,
    first: PathBuf,
) {
    loop {
        tokio::time::sleep(SUPERVISE_INTERVAL).await;
        let mut guard = inner.lock().await;
        if !guard.wanted {
            return;
        }
        let alive = guard.process.as_mut().is_some_and(|p| p.is_running());
        if alive {
            continue;
        }
        if guard.process.take().is_some() {
            guard.interrupted_count += 1;
            tracing::warn!(
                interrupted_count = guard.interrupted_count,
                "recording_interrupted: capture exited while wanted; starting a new segment"
            );
        }
        let next = segment_path(&first, guard.segments.len() + 1);
        match spawn_capture(&ffmpeg, &rtsp_url, &next) {
            Ok(p) => {
                tracing::info!(path = %next.display(), "recording_segment_started");
                guard.process = Some(p);
                guard.segments.push(next);
            }
            Err(e) => {
                tracing::warn!(error = %e, "recording_segment_spawn_failed; retrying");
            }
        }
    }
}

/// True when `program` can be run: an explicit path must be a file; a bare
/// name must resolve on `PATH` (the Python `shutil.which` check).
pub(super) fn program_present(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(program).is_file())
}

/// The free bytes on the volume holding `dir`, or `None` when the stat fails (a
/// `None` is treated as "do not block" at the call site, matching the Python
/// `shutil.disk_usage` `except OSError` path). Linux uses `statvfs`; off Linux
/// the dev host returns `None` so a test never refuses on a disk-space guard.
#[cfg(target_os = "linux")]
pub(super) fn free_bytes(dir: &Path) -> Option<u64> {
    let stat = nix::sys::statvfs::statvfs(dir).ok()?;
    Some(stat.blocks_available() as u64 * stat.fragment_size() as u64)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn free_bytes(_dir: &Path) -> Option<u64> {
    None
}

/// A timestamped MP4 filename, with a sanitised hint appended. Matches the Python
/// recorder's `%Y-%m-%dT%H-%M-%S` UTC stamp + `_<hint>` (alnum / `-` / `_` only,
/// truncated to 48 chars). A hint that sanitises to empty is dropped.
pub(super) fn generate_filename(hint: Option<&str>) -> String {
    let ts = utc_filename_stamp();
    match hint {
        Some(h) if !h.is_empty() => {
            let safe: String = h
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .take(48)
                .collect();
            if safe.is_empty() {
                format!("{ts}.mp4")
            } else {
                format!("{ts}_{safe}.mp4")
            }
        }
        _ => format!("{ts}.mp4"),
    }
}

/// The current UTC time as the recorder's `%Y-%m-%dT%H-%M-%S` filename stamp.
fn utc_filename_stamp() -> String {
    use time::format_description::FormatItem;
    use time::macros::format_description;
    const FMT: &[FormatItem<'_>] =
        format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]");
    time::OffsetDateTime::now_utc()
        .format(FMT)
        .unwrap_or_else(|_| "0000-00-00T00-00-00".to_string())
}

/// The current UTC time as an ISO-8601 timestamp with a `+00:00` offset, matching
/// the Python `datetime.now(timezone.utc).isoformat()` the start/stop returned.
pub(super) fn now_iso8601() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map(|s| {
            // RFC3339 renders UTC as a trailing `Z`; Python's isoformat renders
            // `+00:00`. Match the Python form so the wire value is identical.
            if let Some(stripped) = s.strip_suffix('Z') {
                format!("{stripped}+00:00")
            } else {
                s
            }
        })
        .unwrap_or_else(|_| "1970-01-01T00:00:00+00:00".to_string())
}

/// Round a duration to 2 decimal places, matching the Python `round(x, 2)` the
/// stop summary used.
pub(super) fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
