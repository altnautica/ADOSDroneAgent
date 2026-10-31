//! Display, GPIO and video-source host methods.

use super::*;

impl RealHost {
    pub(super) fn serve_display_page_set(
        &self,
        plugin_id: &str,
        session: u64,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        // Parse the request into the display-page shape, then atomically write
        // the sidecar the reserved page reads. The dispatch gate already
        // enforced the display capability before this runs.
        let page = parse_display_page(args)?;
        let rows = page.rows.len();
        let zones = page.zones.len();
        // The page is one shared surface: while another plugin's connection
        // owns it, this write is refused rather than overwriting that page.
        // The owner is held for the write, so two plugins racing cannot both
        // land a page.
        let mut owner = self
            .display_page_owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((holder, _)) = owner.as_ref().filter(|(holder, _)| holder != plugin_id) {
            return Err(HostError::Rpc(format!(
                "the display page is owned by plugin {holder}"
            )));
        }
        *owner = Some((plugin_id.to_string(), session));
        if let Err(e) = write_display_page(&self.display_page_path, &page) {
            tracing::warn!(
                plugin_id = %plugin_id,
                path = %self.display_page_path.display(),
                error = %e,
                "display page write failed"
            );
            // A write failure is a graceful-degrade response, not a gate
            // failure (matches the not_available shape the other host methods
            // return when their backing surface is unavailable).
            return Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (Value::from("method"), Value::from("display.page.set")),
                (
                    Value::from("reason"),
                    Value::from("display page write failed"),
                ),
            ]));
        }
        Ok(Value::Map(vec![
            (Value::from("set"), Value::Boolean(true)),
            (Value::from("rows"), Value::Integer((rows as i64).into())),
            (Value::from("zones"), Value::Integer((zones as i64).into())),
        ]))
    }

    pub(super) async fn serve_gpio_output_set(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        // Build the service's `set` request from the validated args, then forward
        // it to the GPIO-output command socket. The dispatch gate already enforced
        // the GPIO-output capability before this runs.
        let pin = arg_i64(args, "pin")
            .ok_or_else(|| HostError::Rpc("pin must be an integer".to_string()))?;
        let level = arg_str(args, "level").filter(|l| *l == "high" || *l == "low");
        let Some(level) = level else {
            return Err(HostError::Rpc(
                "level must be \"high\" or \"low\"".to_string(),
            ));
        };
        let chip = arg_i64(args, "chip").unwrap_or(0);
        let req = serde_json::json!({"op": "set", "chip": chip, "pin": pin, "level": level});
        Ok(self.forward_gpio(req, "gpio.output.set").await)
    }

    pub(super) async fn serve_gpio_buzzer_beep(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        // Build the service's `beep` request from the validated args. The service
        // clamps the pattern into the safe bounds, so the host forwards the raw
        // values verbatim and lets the single owner enforce the ceiling.
        let pin = arg_i64(args, "pin")
            .ok_or_else(|| HostError::Rpc("pin must be an integer".to_string()))?;
        let on_ms = arg_i64(args, "on_ms")
            .ok_or_else(|| HostError::Rpc("on_ms must be an integer".to_string()))?;
        let cycles = arg_i64(args, "cycles")
            .ok_or_else(|| HostError::Rpc("cycles must be an integer".to_string()))?;
        let chip = arg_i64(args, "chip").unwrap_or(0);
        let mut req = serde_json::json!({
            "op": "beep", "chip": chip, "pin": pin, "on_ms": on_ms, "cycles": cycles,
        });
        // Optional carrier/envelope fields ride through when present.
        if let Some(off_ms) = arg_i64(args, "off_ms") {
            req["off_ms"] = off_ms.into();
        }
        if let Some(freq_hz) = arg_i64(args, "freq_hz") {
            req["freq_hz"] = freq_hz.into();
        }
        if let Some(duty_pct) = arg_i64(args, "duty_pct") {
            req["duty_pct"] = duty_pct.into();
        }
        Ok(self.forward_gpio(req, "gpio.buzzer.beep").await)
    }

    pub(super) async fn serve_video_source_set(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        // Build the supervisor's video-source request from the validated args,
        // then forward it to the supervisor's video command socket. The dispatch
        // gate already enforced the video-source capability before this runs.
        let cameras = map_get(args, "cameras")
            .and_then(|v| serde_json::to_value(v).ok())
            .filter(|v| v.is_array())
            .ok_or_else(|| HostError::Rpc("cameras must be an array".to_string()))?;
        let legs = cameras.as_array().expect("filtered on is_array");
        if legs.is_empty() {
            return Err(HostError::Rpc("cameras must not be empty".to_string()));
        }
        // A leg with no id or no source cannot be served, so reject the whole
        // list rather than write a half-usable config (never advertise
        // a stream the pipeline can't actually serve).
        for leg in legs {
            let id = leg.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let source = leg.get("source").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() || source.is_empty() {
                return Err(HostError::Rpc(
                    "each camera needs a non-empty id and source".to_string(),
                ));
            }
        }
        // Attribute the declared legs to this plugin so the supervisor's
        // merge-by-owner persist replaces only this plugin's legs and preserves
        // the operator's (and other plugins') legs.
        let req = serde_json::json!({
            "op": "video.source.set",
            "cameras": cameras,
            "owner": plugin_id,
        });
        Ok(self.forward_video(req, "video.source.set").await)
    }
}
