//! The video-latency projection from the store metrics or the live LCD file.

use super::*;

/// The `video.latency.*` metric → route-key map. Each store metric maps back to the
/// JSON key the route returns.
pub(super) const LATENCY_METRICS: [(&str, &str); 3] = [
    ("video.latency.glass_ms", "latency_ms"),
    ("video.latency.ewma_ms", "ewma_ms"),
    ("video.latency.samples", "samples"),
];

/// Reconstruct the `/video/latency` route body from the store.
///
/// Maps the `video.latency.*` metrics back to the route keys and reads the `source` off
/// the latest `video.latency_source` event, falling back to `"sei"` when that event is
/// not in the window. Returns `None` when neither the glass-to-glass sample nor the
/// sample count is present WITHIN [`LATENCY_STALE_AFTER_S`] (the SEI probe is
/// disabled, has produced nothing, or stopped producing), so the route degrades to
/// the live read — which reports `stale` or `unavailable` rather than a number.
///
/// The freshness argument is the same one the live read makes, and it has to be
/// made here too: the store query is a row-count page, not a time window, so the
/// last sample the probe ever recorded stays inside it for as long as the box is
/// quiet enough not to push it out.
pub(super) async fn latest_video_latency(state: &AppState) -> Option<Value> {
    let names: Vec<&str> = LATENCY_METRICS.iter().map(|(m, _)| *m).collect();
    let metrics = latest_metrics(state, &names, LATENCY_STALE_AFTER_S).await;
    let glass = metric_value(metrics.as_ref(), "video.latency.glass_ms");
    let samples = metric_value(metrics.as_ref(), "video.latency.samples");
    if glass.is_none() && samples.is_none() {
        return None;
    }
    let ewma = metric_value(metrics.as_ref(), "video.latency.ewma_ms");
    let source = latest_event_field(state, "video.latency_source", "source")
        .await
        .unwrap_or_else(|| json!("sei"));

    Some(json!({
        "latency_ms": glass.map(Value::from).unwrap_or(Value::Null),
        "ewma_ms": ewma.map(Value::from).unwrap_or(Value::Null),
        "samples": samples.map(|s| json!(s as i64)).unwrap_or(Value::Null),
        "source": source,
    }))
}

/// The live SEI-latency read: the file-backed fallback, resolving the
/// runtime-dir-relative `lcd-latency.json` path. The projection itself lives in
/// [`project_latency_live`] (path-injectable for tests).
pub(super) fn read_latency_live() -> Value {
    project_latency_live(&lcd_latency_path())
}

/// Beyond this age the `lcd-latency.json` snapshot can no longer describe the
/// pipeline NOW, so the route reports it as stale rather than as a reading.
///
/// The registry window for `lcd-latency`. The producing tap rewrites the file
/// once per second, and only on a poll where it actually parsed a new SEI marker
/// (`_PERSIST_INTERVAL_S = 1.0` in `src/ados/services/video/sei_tap.py`), so a
/// probe that is genuinely in the byte path refreshes it ten times inside this
/// window while a probe that has been switched off stops refreshing it
/// immediately. The durable store's sidecar tailer applies the same registry
/// window to this exact file, and [`LINK_STALE_AFTER_S`] applies the same
/// ceiling to `wfb-stats.json`, so the store path, the live path and the link
/// block all flip at one moment instead of three.
pub(super) const LATENCY_STALE_AFTER_S: f64 = paths::LCD_LATENCY_MAX_AGE_MS as f64 / 1000.0;

/// A window in seconds as a [`Duration`].
pub(super) fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

/// Project a latency state file into the route body.
///
/// Reads `lcd-latency.json` when present and fresh. The degraded shapes, all
/// `latency_ms: null`, are distinguished by `source` so a consumer learns which
/// one it hit: `"unavailable"` (no file — the probe never ran), `"stale"` (a file
/// older than [`LATENCY_STALE_AFTER_S`] or future-dated, carrying its `age_s` when
/// it can be established — the probe ran and stopped), and `"read_failed"` (a
/// read/parse error, or a body that is not a JSON object).
///
/// The stale branch is the load-bearing one. Nothing deletes this file when the
/// SEI probe is disabled or `ados-video` restarts without it, so an ungated read
/// kept serving the last number the probe ever produced as a current one — a
/// surface reporting a known-false value, and one an operator could only
/// disprove by going to the box and checking whether `ados-video` ran with the
/// SEI injector enabled.
///
/// The display service's framebuffer-writer counters live in their own
/// `lcd-fb-stats.json`, so every body in this file is a latency snapshot;
/// `latency_ms: null` is a running probe with no sample yet and reads as `"sei"`.
pub(super) fn project_latency_live(path: &Path) -> Value {
    let now = SystemTime::now();
    let window = Some(secs(LATENCY_STALE_AFTER_S));
    let map = match read_fresh_at::<Map<String, Value>>(path, window, now) {
        Reading::Fresh(map) => map,
        Reading::Absent => return json!({"latency_ms": null, "source": "unavailable"}),
        Reading::Malformed(_) => return json!({"latency_ms": null, "source": "read_failed"}),
        Reading::Stale(_) => {
            let age_s = file_age(path, now).map(|d| (d.as_secs_f64() * 10.0).round() / 10.0);
            return json!({
                "latency_ms": null,
                "ewma_ms": null,
                "samples": null,
                "source": "stale",
                "age_s": age_s,
            });
        }
    };
    // `ewma_ms` prefers `latency_ewma_ms`, then `ewma_ms`. `source` defaults "sei".
    let ewma = map
        .get("latency_ewma_ms")
        .or_else(|| map.get("ewma_ms"))
        .cloned()
        .unwrap_or(Value::Null);
    let source = map
        .get("source")
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!("sei"));
    json!({
        "latency_ms": map.get("latency_ms").cloned().unwrap_or(Value::Null),
        "ewma_ms": ewma,
        "samples": map.get("samples").cloned().unwrap_or(Value::Null),
        "source": source,
    })
}
