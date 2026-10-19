//! `gnss-cmd.sock`: one newline-delimited JSON request per connection.
//!
//! `{"cmd":"set_base","base":{"mode":"off"|"survey"|"fixed",...}}` →
//! `{"ok":true,"base":<base object>}` or
//! `{"ok":false,"error":"E_GNSS_...","detail":"..."}`. The mode is written
//! to the base receiver at once (RAM layer; `off` sends TMODE 0 and stops the
//! RTCM3 output). This service does not write `gnss.base` to the agent
//! config: `PUT /api/v1/gnss/base` saves it after this reply is `ok`.

use std::path::{Path, PathBuf};

use ados_protocol::shutdown::Shutdown;
use ados_sensorbus::time::mono_ns;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::base::{BaseMode, Llh};
use crate::service::{Shared, E_BAD_REQUEST};
use crate::status::linked_peer_count;

/// Largest request line accepted.
const MAX_LINE: u64 = 4096;

fn err(code: &str, detail: impl Into<String>) -> Value {
    json!({"ok": false, "error": code, "detail": detail.into()})
}

/// Validate a PUT /api/v1/gnss/base body. Survey fields left out take the
/// current survey settings.
pub fn parse_base(body: &Value, defaults: (u32, f64)) -> Result<BaseMode, String> {
    let num = |k: &str| body.get(k).and_then(Value::as_f64);
    match body.get("mode").and_then(Value::as_str) {
        Some("off") => Ok(BaseMode::Off),
        Some("survey") => {
            let min_dur = match body.get("min_dur_s") {
                None | Some(Value::Null) => defaults.0 as f64,
                Some(v) => v.as_f64().ok_or("min_dur_s must be a number")?,
            };
            let acc = match body.get("accuracy_m") {
                None | Some(Value::Null) => defaults.1,
                Some(v) => v.as_f64().ok_or("accuracy_m must be a number")?,
            };
            if min_dur < 1.0 || min_dur > u32::MAX as f64 || min_dur.fract() != 0.0 {
                return Err("min_dur_s must be a whole number of seconds >= 1".into());
            }
            if !acc.is_finite() || acc <= 0.0 {
                return Err("accuracy_m must be > 0".into());
            }
            Ok(BaseMode::Survey {
                min_dur_s: min_dur as u32,
                accuracy_m: acc,
            })
        }
        Some("fixed") => {
            let (Some(lat), Some(lon), Some(h)) =
                (num("lat_deg"), num("lon_deg"), num("h_ellipsoid_m"))
            else {
                return Err("fixed mode needs lat_deg, lon_deg and h_ellipsoid_m".into());
            };
            if !(-90.0..=90.0).contains(&lat) {
                return Err("lat_deg must be in [-90, 90]".into());
            }
            if !(-180.0..=180.0).contains(&lon) {
                return Err("lon_deg must be in [-180, 180]".into());
            }
            if !h.is_finite() {
                return Err("h_ellipsoid_m must be finite".into());
            }
            Ok(BaseMode::Fixed(Llh {
                lat_deg: lat,
                lon_deg: lon,
                h_ellipsoid_m: h,
            }))
        }
        _ => Err("mode must be off, survey or fixed".into()),
    }
}

/// Handle one request line.
pub fn handle(shared: &Shared, line: &str, peers_path: &Path) -> Value {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return err(E_BAD_REQUEST, format!("request is not JSON: {e}")),
    };
    match req.get("cmd").and_then(Value::as_str) {
        Some("set_base") => {}
        other => return err(E_BAD_REQUEST, format!("unknown cmd {other:?}")),
    }
    let body = req.get("base").cloned().unwrap_or(Value::Null);
    let defaults = shared.lock().base_survey_defaults();
    let mode = match parse_base(&body, defaults) {
        Ok(m) => m,
        Err(detail) => return err(E_BAD_REQUEST, detail),
    };
    let targets = linked_peer_count(peers_path);
    let now = mono_ns();
    let mut svc = shared.lock();
    match svc.set_base(mode, now) {
        Ok(()) => json!({"ok": true, "base": svc.base_out(now, targets)}),
        Err((code, detail)) => err(code, detail),
    }
}

/// Serve the command socket until shutdown.
pub async fn serve(shared: Shared, sock: PathBuf, peers_path: PathBuf, shutdown: Shutdown) {
    let listener = match ados_protocol::ipc::bind_command_socket(&sock, 0o660) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(path = %sock.display(), error = %e, "gnss_cmd_socket_bind_failed");
            return;
        }
    };
    loop {
        let stream = tokio::select! {
            _ = shutdown.wait() => break,
            r = listener.accept() => match r {
                Ok((s, _)) => s,
                Err(_) => continue,
            },
        };
        let shared = shared.clone();
        let peers_path = peers_path.clone();
        tokio::spawn(async move {
            let (rd, mut wr) = stream.into_split();
            let mut line = String::new();
            let mut reader = BufReader::new(rd).take(MAX_LINE);
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            let reply = handle(&shared, line.trim(), &peers_path);
            let mut out = reply.to_string();
            out.push('\n');
            let _ = wr.write_all(out.as_bytes()).await;
            let _ = wr.shutdown().await;
        });
    }
    let _ = std::fs::remove_file(&sock);
}
