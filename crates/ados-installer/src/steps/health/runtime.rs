//! The health gate's runtime checks, both best-effort: whether the video
//! pipeline reaches `streaming`, and whether any agent unit sits in `failed`.
//! Each miss is recorded non-required, so the install reads `degraded` (never
//! `failed`, never rolled back) and names what is down instead of `ok`.

use std::time::Duration;

use crate::binaries;
use crate::ctx::Ctx;
use ados_term::exec;

/// How many camera-state reads the video gate takes, [`VIDEO_POLL`] apart.
const VIDEO_POLLS: u32 = 30;
const VIDEO_POLL: Duration = Duration::from_secs(2);

/// How many consecutive reads must agree before `missing` or `error` is
/// final. The sidecar the restarted video service replaces can still be fresh
/// for a few seconds, so one read of the previous run's verdict decides
/// nothing.
const SETTLED_READS: u32 = 3;

/// What the video pipeline settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum VideoVerdict {
    Streaming,
    /// No camera attached: nothing to stream, nothing wrong.
    NoCamera,
    /// The pipeline reports an error, with its reason.
    Error(String),
    /// Never reached `streaming`; the last pipeline state read.
    NotStreaming(String),
}

/// Poll `read` (`(state, pipeline_state, pipeline_reason)`, `None` when there
/// is no fresh reading) up to `polls` times with `sleep` between, until the
/// pipeline streams or a `missing` camera or an `error` holds for
/// [`SETTLED_READS`] reads in a row.
pub(super) fn video_verdict(
    mut read: impl FnMut() -> Option<(String, String, Option<String>)>,
    polls: u32,
    mut sleep: impl FnMut(),
) -> VideoVerdict {
    let mut last = String::from("absent");
    let mut held: Option<VideoVerdict> = None;
    let mut reads = 0;
    for poll in 0..polls {
        if poll > 0 {
            sleep();
        }
        let Some((state, pipeline, reason)) = read() else {
            held = None;
            continue;
        };
        if pipeline == "streaming" {
            return VideoVerdict::Streaming;
        }
        let verdict = if state == "missing" {
            Some(VideoVerdict::NoCamera)
        } else if pipeline == "error" {
            Some(VideoVerdict::Error(
                reason.unwrap_or_else(|| "unknown".to_string()),
            ))
        } else {
            None
        };
        if !pipeline.is_empty() {
            last = pipeline;
        }
        if verdict.is_some() && verdict == held {
            reads += 1;
        } else {
            held = verdict;
            reads = 1;
        }
        if reads >= SETTLED_READS {
            if let Some(v) = held {
                return v;
            }
        }
    }
    VideoVerdict::NotStreaming(last)
}

/// One fresh read of the camera-state sidecar.
fn read_camera_state() -> Option<(String, String, Option<String>)> {
    let ados_protocol::sidecar::Reading::Fresh(v) = ados_protocol::sidecar::read_fresh::<
        serde_json::Value,
    >(
        "camera-state", std::time::SystemTime::now()
    ) else {
        return None;
    };
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    Some((
        field("state")?,
        field("pipeline_state").unwrap_or_default(),
        field("pipeline_reason"),
    ))
}

/// Record a video pipeline that is not streaming. Skipped where the profile
/// ships no video service or the service is not running (video disabled: the
/// unit idles and exits 0).
pub(super) fn check_video(ctx: &mut Ctx) {
    if !binaries::profile_ships(&ctx.profile, "ados-video") {
        return;
    }
    if exec::run("systemctl", &["is-active", "ados-video.service"])
        .stdout
        .trim()
        != "active"
    {
        return;
    }
    match video_verdict(read_camera_state, VIDEO_POLLS, || {
        std::thread::sleep(VIDEO_POLL)
    }) {
        VideoVerdict::Streaming | VideoVerdict::NoCamera => {}
        VideoVerdict::Error(reason) => {
            tracing::warn!(reason = %reason, "video pipeline reports an error");
            ctx.failures
                .record_with_reason("video-pipeline-error", false, &reason);
        }
        VideoVerdict::NotStreaming(state) => {
            let waited = VIDEO_POLL.as_secs() * u64::from(VIDEO_POLLS);
            tracing::warn!(pipeline_state = %state, "video pipeline never reached streaming");
            ctx.failures.record_with_reason(
                "video-pipeline-not-streaming",
                false,
                &format!("pipeline_state={state} after {waited} s"),
            );
        }
    }
}

/// The unit names in `systemctl list-units --plain --no-legend` output.
pub(super) fn failed_unit_names(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// Record every agent unit systemd lists as failed.
pub(super) fn sweep_failed_units(ctx: &mut Ctx) {
    let out = exec::run(
        "systemctl",
        &[
            "list-units",
            "--state=failed",
            "--plain",
            "--no-legend",
            "--no-pager",
            "ados-*",
        ],
    )
    .stdout;
    for name in failed_unit_names(&out) {
        tracing::warn!(unit = %name, "agent unit is failed");
        ctx.failures.record(&format!("unit-failed:{name}"), false);
    }
}
