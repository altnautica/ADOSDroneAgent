//! **`POST /api/video/config`**: apply video / radio link-tuning knobs.
//!
//! Body `{bitrate_kbps?, fec_k?, fec_n?, mcs?, auto?, tier_idx?, preset?}`.
//! Every field is optional and applied independently, so a partial request
//! leaves the rest untouched and an empty one is a no-op. The answer is the
//! `GET /api/video/config` body, re-read after the write, plus a `warnings` list
//! naming every knob that did not take, so a partial success is visible.
//!
//! The FEC, MCS, preset and link-tier knobs go to the radio's data-plane command
//! socket (`wfb-cmd.sock`), which applies them to the running transmitter. A knob
//! the radio applied is persisted to `video.wfb` in the agent config so it
//! survives a restart; a knob it did not apply is never persisted. The encoder
//! takes its bitrate from the attention profile and the adaptive controller, so
//! `bitrate_kbps` is refused with a warning rather than pretended.
//!
//! Warnings: `bitrate_not_settable_on_this_surface`; `<op>_failed` when the radio
//! refused an op (`set_fec`, `set_mcs`, `set_preset`, `set_tier_manual`,
//! `set_tier_auto`); `set_manual_tier_failed` for a tier index past the ladder;
//! `radio_unavailable` when the socket did not answer; `persist_failed` when an
//! applied knob could not be saved.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use ados_radio::bitrate::DEFAULT_TIERS;
use ados_radio::config::link_preset_trio;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use serde_norway::Value as Yaml;

use crate::config_store::{section_path, update_config};
use crate::ipc::cmd;
use crate::routes::detail;
use crate::routes::video::{video_config_body, VideoConfig};
use crate::routes::wfb_write::{config_yaml_path, wfb_cmd_sock};

/// How long one tuning op may take. An FEC / MCS / manual-tier change can respawn
/// the transmitter before the radio replies, so this is longer than a pure read.
const RADIO_TUNE_TIMEOUT: Duration = Duration::from_secs(10);

/// The `POST /api/video/config` body. Unknown fields are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct VideoConfigBody {
    #[serde(default)]
    bitrate_kbps: Option<i64>,
    #[serde(default)]
    fec_k: Option<i64>,
    #[serde(default)]
    fec_n: Option<i64>,
    #[serde(default)]
    mcs: Option<i64>,
    #[serde(default)]
    auto: Option<bool>,
    #[serde(default)]
    tier_idx: Option<i64>,
    #[serde(default)]
    preset: Option<String>,
}

impl VideoConfigBody {
    /// Per-field bounds; the first violation is the `422` message.
    fn validate(&self) -> Result<(), String> {
        fn within(name: &str, value: Option<i64>, lo: i64, hi: i64) -> Result<(), String> {
            match value {
                Some(v) if v < lo || v > hi => Err(format!("{name} must be between {lo} and {hi}")),
                _ => Ok(()),
            }
        }
        within("bitrate_kbps", self.bitrate_kbps, 500, 12000)?;
        within("fec_k", self.fec_k, 1, 64)?;
        within("fec_n", self.fec_n, 2, 128)?;
        within("mcs", self.mcs, 0, 7)?;
        within("tier_idx", self.tier_idx, 0, 8)?;
        if let Some(preset) = &self.preset {
            if link_preset_trio(preset).is_none() {
                return Err("preset must be one of conservative, balanced, aggressive".to_string());
            }
        }
        Ok(())
    }
}

/// `POST /api/video/config` → the config snapshot plus `warnings`; `422` on an
/// out-of-range field.
pub async fn post_video_config(Json(body): Json<VideoConfigBody>) -> Response {
    apply_video_config(&wfb_cmd_sock(), &config_yaml_path(), &body).await
}

/// The write against an explicit radio socket and config path, so a test can
/// point both at a temp dir.
async fn apply_video_config(socket: &Path, config_path: &Path, body: &VideoConfigBody) -> Response {
    if let Err(message) = body.validate() {
        return detail(StatusCode::UNPROCESSABLE_ENTITY, message);
    }
    // The configured trio fills whatever half of a knob the request leaves out.
    let (cfg_mcs, cfg_k, cfg_n) = VideoConfig::load_from(config_path).data_plane();
    let mut warnings: Vec<String> = Vec::new();
    let mut persist: BTreeMap<&'static str, Yaml> = BTreeMap::new();

    if body.bitrate_kbps.is_some() {
        warnings.push("bitrate_not_settable_on_this_surface".to_string());
    }

    if body.fec_k.is_some() || body.fec_n.is_some() {
        let k = body.fec_k.unwrap_or(cfg_k);
        let n = body.fec_n.unwrap_or(cfg_n);
        let request = json!({"op": "set_fec", "fec_k": k, "fec_n": n});
        // Persist only a valid ratio (n > k >= 1), the invariant the radio
        // setter enforces, so a ratio it would reject never reaches config.
        if tune(socket, "set_fec", &request, &mut warnings)
            .await
            .is_some()
            && k >= 1
            && n > k
        {
            persist.insert("fec_k", int(k));
            persist.insert("fec_n", int(n));
        }
    }

    if let Some(mcs) = body.mcs {
        let request = json!({"op": "set_mcs", "mcs_index": mcs});
        if tune(socket, "set_mcs", &request, &mut warnings)
            .await
            .is_some()
        {
            persist.insert("mcs_index", int(mcs));
        }
    }

    if let Some((preset, (mcs, k, n))) = body
        .preset
        .as_deref()
        .and_then(|p| link_preset_trio(p).map(|trio| (p, trio)))
    {
        let request = json!({"op": "set_preset", "preset": preset});
        if let Some(reply) = tune(socket, "set_preset", &request, &mut warnings).await {
            // The radio echoes the trio it pinned; the table is the fallback when
            // the echo omits it. Persisting the trio lets the preset survive a
            // restart even where its boot apply is a no-op.
            let fallback = (i64::from(mcs), i64::from(k), i64::from(n));
            let (mcs, k, n) = echoed_trio(&reply).unwrap_or(fallback);
            persist.insert("wfb_link_preset", Yaml::String(preset.to_string()));
            persist.insert("mcs_index", int(mcs));
            persist.insert("fec_k", int(k));
            persist.insert("fec_n", int(n));
        }
    }

    if body.auto.is_some() || body.tier_idx.is_some() {
        apply_tier(
            socket,
            body,
            (cfg_mcs, cfg_k, cfg_n),
            &mut warnings,
            &mut persist,
        )
        .await;
    }

    if !persist.is_empty() {
        let written = update_config(config_path, |root| {
            let wfb = section_path(root, &["video", "wfb"]);
            for (key, value) in &persist {
                wfb.insert(Yaml::String((*key).to_string()), value.clone());
            }
            Ok(())
        });
        if let Err(e) = written {
            tracing::warn!(
                path = %config_path.display(),
                error = %e,
                "video tuning applied to the radio but not persisted; it will revert on restart"
            );
            warnings.push("persist_failed".to_string());
        }
    }

    let mut response = video_config_body(&VideoConfig::load_from(config_path));
    response["warnings"] = json!(warnings);
    (StatusCode::OK, Json(response)).into_response()
}

/// The auto/manual link-tier toggle. `auto: true` re-arms the adaptive
/// controller. A pinned `tier_idx` maps its ladder rung to a manual trio (the
/// rung sets the FEC, the configured MCS stands). `auto: false` with no rung
/// pins the configured trio, so the controller stops stepping without forcing a
/// different rung. `tier_idx` takes precedence over `auto`.
async fn apply_tier(
    socket: &Path,
    body: &VideoConfigBody,
    (cfg_mcs, cfg_k, cfg_n): (i64, i64, i64),
    warnings: &mut Vec<String>,
    persist: &mut BTreeMap<&'static str, Yaml>,
) {
    let manual = |k: i64, n: i64| json!({"op": "set_tier", "mode": "manual", "mcs_index": cfg_mcs, "fec_k": k, "fec_n": n});
    if let Some(idx) = body.tier_idx {
        let Some(rung) = usize::try_from(idx)
            .ok()
            .and_then(|i| DEFAULT_TIERS.get(i).copied())
        else {
            warnings.push("set_manual_tier_failed".to_string());
            return;
        };
        let (k, n) = (i64::from(rung.fec_k), i64::from(rung.fec_n));
        if tune(socket, "set_tier_manual", &manual(k, n), warnings)
            .await
            .is_some()
        {
            // A pinned rung implies adaptive off; its FEC is the new baseline.
            persist.insert("adaptive_bitrate_enabled", Yaml::Bool(false));
            persist.insert("fec_k", int(k));
            persist.insert("fec_n", int(n));
        }
        return;
    }
    let applied = if body.auto == Some(true) {
        let request = json!({"op": "set_tier", "mode": "auto"});
        tune(socket, "set_tier_auto", &request, warnings)
            .await
            .is_some()
    } else {
        tune(socket, "set_tier_manual", &manual(cfg_k, cfg_n), warnings)
            .await
            .is_some()
    };
    if let (true, Some(auto)) = (applied, body.auto) {
        persist.insert("adaptive_bitrate_enabled", Yaml::Bool(auto));
    }
}

/// Send one tuning op to the radio. Returns the reply when the radio applied it
/// (`ok: true`); otherwise names the miss in `warnings`: `<op>_failed` when the
/// radio answered without applying, `radio_unavailable` when it did not answer.
async fn tune(
    socket: &Path,
    op: &str,
    request: &Value,
    warnings: &mut Vec<String>,
) -> Option<Value> {
    match cmd::roundtrip(socket, request, RADIO_TUNE_TIMEOUT).await {
        Ok(reply) if reply.get("ok") == Some(&Value::Bool(true)) => Some(reply),
        Ok(_) => {
            warnings.push(format!("{op}_failed"));
            None
        }
        Err(_) => {
            warnings.push("radio_unavailable".to_string());
            None
        }
    }
}

/// The `(mcs_index, fec_k, fec_n)` trio a preset reply echoes, when all three are
/// integers.
fn echoed_trio(reply: &Value) -> Option<(i64, i64, i64)> {
    let field = |key: &str| reply.get(key).and_then(Value::as_i64);
    Some((field("mcs_index")?, field("fec_k")?, field("fec_n")?))
}

fn int(v: i64) -> Yaml {
    Yaml::Number(v.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// A radio stub that answers every connection with `reply` and records the
    /// request lines it was sent.
    fn radio_stub(sock: &Path, reply: Value) -> tokio::task::JoinHandle<Vec<Value>> {
        let listener = UnixListener::bind(sock).unwrap();
        tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Ok(Ok((conn, _))) =
                tokio::time::timeout(Duration::from_millis(300), listener.accept()).await
            {
                let mut conn = BufReader::new(conn);
                let mut line = String::new();
                conn.read_line(&mut line).await.unwrap();
                seen.push(serde_json::from_str(&line).unwrap());
                let mut out = serde_json::to_vec(&reply).unwrap();
                out.push(b'\n');
                conn.get_mut().write_all(&out).await.unwrap();
            }
            seen
        })
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn persisted_wfb(cfg: &Path) -> Yaml {
        let doc: Yaml = serde_norway::from_str(&std::fs::read_to_string(cfg).unwrap()).unwrap();
        doc["video"]["wfb"].clone()
    }

    #[tokio::test]
    async fn a_knob_the_radio_did_not_take_is_named_and_never_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let body = VideoConfigBody {
            mcs: Some(4),
            bitrate_kbps: Some(3000),
            ..Default::default()
        };
        let resp = apply_video_config(&dir.path().join("absent.sock"), &cfg, &body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(
            json["warnings"],
            json!(["bitrate_not_settable_on_this_surface", "radio_unavailable"])
        );
        assert!(!cfg.exists(), "an unapplied knob must not be persisted");
        // The body is the config snapshot: the configured MCS still reads.
        assert_eq!(json["radio"]["mcs_index"], 1);
    }

    #[tokio::test]
    async fn an_applied_knob_is_persisted_and_read_back_in_the_response() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let seed = "agent:\n  name: my-drone\nvideo:\n  wfb:\n    channel: 149\n";
        std::fs::write(&cfg, seed).unwrap();
        let sock = dir.path().join("wfb-cmd.sock");
        let radio = radio_stub(&sock, json!({"ok": true}));
        let body = VideoConfigBody {
            mcs: Some(4),
            ..Default::default()
        };
        let json = body_json(apply_video_config(&sock, &cfg, &body).await).await;
        assert_eq!(json["warnings"], json!([]));
        assert_eq!(json["radio"]["mcs_index"], 4);
        assert_eq!(persisted_wfb(&cfg)["mcs_index"], Yaml::Number(4.into()));
        assert_eq!(persisted_wfb(&cfg)["channel"], Yaml::Number(149.into()));
        assert_eq!(
            radio.await.unwrap(),
            vec![json!({"op": "set_mcs", "mcs_index": 4})]
        );
    }

    #[tokio::test]
    async fn a_pinned_tier_turns_adaptive_off_and_a_tier_past_the_ladder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let sock = dir.path().join("wfb-cmd.sock");
        let radio = radio_stub(&sock, json!({"ok": true}));

        let rung = DEFAULT_TIERS[1];
        let pinned = VideoConfigBody {
            tier_idx: Some(1),
            ..Default::default()
        };
        let json = body_json(apply_video_config(&sock, &cfg, &pinned).await).await;
        assert_eq!(json["warnings"], json!([]));
        let wfb = persisted_wfb(&cfg);
        assert_eq!(wfb["adaptive_bitrate_enabled"], Yaml::Bool(false));
        assert_eq!(wfb["fec_n"], Yaml::Number(i64::from(rung.fec_n).into()));

        // Within the accepted 0..=8 bound but past the four-rung ladder: refused
        // before the radio is asked.
        let beyond = VideoConfigBody {
            tier_idx: Some(DEFAULT_TIERS.len() as i64),
            ..Default::default()
        };
        let json = body_json(apply_video_config(&sock, &cfg, &beyond).await).await;
        assert_eq!(json["warnings"], json!(["set_manual_tier_failed"]));

        assert_eq!(
            radio.await.unwrap(),
            vec![json!({"op": "set_tier", "mode": "manual", "mcs_index": 1,
                        "fec_k": rung.fec_k, "fec_n": rung.fec_n})]
        );
    }

    #[tokio::test]
    async fn an_out_of_range_field_is_a_422_before_the_radio_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let body = VideoConfigBody {
            mcs: Some(8),
            ..Default::default()
        };
        let resp = apply_video_config(&dir.path().join("absent.sock"), &cfg, &body).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!cfg.exists());
    }
}
