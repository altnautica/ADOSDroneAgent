//! The [`HostServices`] implementation: one wire handler per host method.

use super::*;

impl HostServices for RealHost {
    fn telemetry_extend(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let channel = arg_str(args, "channel").filter(|c| !c.is_empty());
        let Some(channel) = channel else {
            return Err(HostError::Rpc(
                "channel must be a non-empty string".to_string(),
            ));
        };
        // payload defaults to an empty map; a non-map payload is an _RpcError.
        let payload = match map_get(args, "payload") {
            None | Some(Value::Nil) => Value::Map(vec![]),
            Some(v @ Value::Map(_)) => v.clone(),
            Some(_) => return Err(HostError::Rpc("payload must be a mapping".to_string())),
        };
        // The merged channel is mirrored into the plugin's state sidecar by
        // the server, which is what serves it to the GCS.
        let _ = (plugin_id, payload);
        Ok(Value::Map(vec![
            (Value::from("merged"), Value::Boolean(true)),
            (Value::from("channel"), Value::from(channel)),
        ]))
    }

    fn telemetry_state_stream(&self, _plugin_id: &str) -> Option<broadcast::Receiver<Arc<Value>>> {
        // One connection to the state socket for the whole host, shared by
        // every subscriber, started on first use so a node whose plugins never
        // read telemetry never opens it. A socket that is absent or closes is
        // retried on the fixed host-reader interval with no cap.
        //
        // Always `Some`: no flight controller (a workstation, a bench) is a
        // resting state, so the subscription succeeds and stays quiet.
        let tx = self.state_reader.get_or_init(|| {
            let (tx, _rx) = broadcast::channel(STATE_BROADCAST_DEPTH);
            let path = self.vehicle_state_sock.clone();
            let task_tx = tx.clone();
            tokio::spawn(async move {
                crate::button_client::reconnect_forever("vehicle-state", || {
                    state_pump(&path, &task_tx)
                })
                .await;
            });
            tx
        });
        Some(tx.subscribe())
    }

    fn mavlink_send(
        &self,
        plugin_id: &str,
        args: &Value,
        granted_caps: &BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        // Order of operations: argument validation, then the frame scan, then
        // the pose-inject capability gate, then the declared component-id VIO
        // gate + reservation check, then the header component-id gates, then the
        // router-None / send. The inline gates run INSIDE the handler, after
        // validation, so a malformed request fails validation before any
        // capability check.

        // 1. Validate msg_bytes (bytes/bytearray or list-of-ints; a string is
        //    rejected). Only Binary/Array are accepted as bytes-bearing.
        let msg_value = arg_owned(args, "msg_bytes");
        let msg_bytes = match &msg_value {
            Value::Array(_) | Value::Binary(_) => {
                coerce_msg_bytes(&msg_value).map_err(HostError::Rpc)?
            }
            _ => return Err(HostError::Rpc("msg_bytes must be bytes".to_string())),
        };
        if msg_bytes.is_empty() {
            return Err(HostError::Rpc("msg_bytes must be non-empty".to_string()));
        }

        // 2. Walk EVERY frame in the buffer: the router forwards it whole, so a
        //    benign leading frame would otherwise speak for the frames behind it.
        //    A buffer the walk cannot classify is refused whatever the caller
        //    holds, because an unparseable frame can hide a whole frame inside.
        let Some(frames) = scan_outbound(&msg_bytes) else {
            return Err(HostError::Rpc(
                "msg_bytes is not a run of whole, valid MAVLink frames".to_string(),
            ));
        };

        // 3. Pose-inject gate: rejects ungranted callers regardless of the
        //    dispatch-level mavlink.write.
        if frames.requires_pose_cap && !granted_caps.contains("estimator.pose.inject") {
            return Err(HostError::CapabilityDenied(
                "estimator.pose.inject".to_string(),
            ));
        }

        // 4. Declared component-id: VIO cap gate, then reservation check, then
        //    every frame header must carry that id.
        if map_has(args, "component_id") {
            let comp = arg_owned(args, "component_id");
            if !matches!(comp, Value::Nil) {
                let comp_id = coerce_component_id(&comp).map_err(HostError::Rpc)?;
                if VIO_COMPONENT_IDS.contains(&comp_id)
                    && !granted_caps.contains("mavlink.component.vio")
                {
                    return Err(HostError::CapabilityDenied(
                        "mavlink.component.vio".to_string(),
                    ));
                }
                if !self
                    .components
                    .lock()
                    .expect("components mutex poisoned")
                    .is_registered(plugin_id, comp_id)
                {
                    return Err(HostError::Rpc(format!(
                        "component_id {comp_id} not reserved by {plugin_id}; \
                         call mavlink.register_component first"
                    )));
                }
                if let Some(header) = frames
                    .component_ids
                    .iter()
                    .find(|&&h| i64::from(h) != comp_id)
                {
                    return Err(HostError::Rpc(format!(
                        "component_id {comp_id} does not match the frame header component id {header}"
                    )));
                }
            }
        }

        // 5. Header component-ids: what each frame claims to be is gated, not
        //    what the caller declared. A VIO id needs the VIO cap and this
        //    plugin's reservation; an id another plugin reserved is refused.
        self.check_frame_components(plugin_id, &frames.component_ids, granted_caps)?;

        match &self.mavlink {
            None => Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (Value::from("method"), Value::from("mavlink.send")),
            ])),
            Some(client) => {
                if let Err(e) = client.send(&msg_bytes) {
                    return Ok(send_refused(e));
                }
                Ok(Value::Map(vec![
                    (Value::from("sent"), Value::Boolean(true)),
                    (
                        Value::from("len"),
                        Value::Integer((msg_bytes.len() as i64).into()),
                    ),
                ]))
            }
        }
    }

    fn msp_send(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        // The MSP byte plane is opaque: validate that msg_bytes are bytes and
        // non-empty, then forward raw. No pose-inject scan or component gate (MSP
        // carries no such frames); the dispatch-level msp.write cap is the whole
        // gate. Failures are reported, matching mavlink.send.
        let msg_value = arg_owned(args, "msg_bytes");
        let msg_bytes = match &msg_value {
            Value::Array(_) | Value::Binary(_) => {
                coerce_msg_bytes(&msg_value).map_err(HostError::Rpc)?
            }
            _ => return Err(HostError::Rpc("msg_bytes must be bytes".to_string())),
        };
        if msg_bytes.is_empty() {
            return Err(HostError::Rpc("msg_bytes must be non-empty".to_string()));
        }
        match self.msp.as_ref() {
            None => Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (Value::from("method"), Value::from("msp.send")),
            ])),
            Some(client) => {
                if let Err(e) = client.send(&msg_bytes) {
                    return Ok(send_refused(e));
                }
                Ok(Value::Map(vec![
                    (Value::from("sent"), Value::Boolean(true)),
                    (
                        Value::from("len"),
                        Value::Integer((msg_bytes.len() as i64).into()),
                    ),
                ]))
            }
        }
    }

    fn mavlink_tunnel_send(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        // Validate the request, build the single TUNNEL frame, and write it to
        // the MAVLink socket. The dispatch gate already enforced the tunnel
        // capability before this runs. The tunnel is a transparent opaque pipe:
        // this stamps no application semantics on the payload, so any per-payload
        // HMAC/replay lives inside the bytes the caller supplied.

        // payload_type is required and must be a private (application) type; the
        // builder re-checks the floor, so an out-of-range type is refused twice.
        let payload_type = arg_int_required::<u16>(args, "payload_type")?;

        // payload accepts the same shapes as mavlink.send's msg_bytes (a binary
        // value or a list of byte-ints). It may be empty (a zero-byte tunnel
        // ping). A wrong type is rejected, never silently coerced.
        let payload_value = arg_owned(args, "payload");
        let payload = match &payload_value {
            Value::Binary(_) | Value::Array(_) => {
                coerce_msg_bytes(&payload_value).map_err(HostError::Rpc)?
            }
            Value::Nil => Vec::new(),
            _ => return Err(HostError::Rpc("payload must be bytes".to_string())),
        };
        if payload.len() > ados_protocol::mavlink::TUNNEL_MAX_PAYLOAD {
            return Err(HostError::Rpc(format!(
                "payload is {} bytes, exceeds the {}-byte TUNNEL limit",
                payload.len(),
                ados_protocol::mavlink::TUNNEL_MAX_PAYLOAD
            )));
        }

        // Target: explicit args, else the autopilot observed on the router link.
        let (target_system, target_component) = command_target(args, self.fc_identity.get())?;

        let header = ados_protocol::mavlink::MavHeader {
            system_id: TUNNEL_SOURCE_SYSTEM_ID,
            component_id: TUNNEL_SOURCE_COMPONENT_ID,
            // Fire-and-forget; the router stamps its own sequence on its own send
            // path, and a client-written frame does not require a specific one.
            sequence: 0,
        };
        // The builder enforces the private-type floor and the payload width, so a
        // bad request is a clean Rpc error rather than a malformed frame.
        let frame = ados_protocol::mavlink::build_tunnel_v2(
            header,
            payload_type,
            target_system,
            target_component,
            &payload,
        )
        .map_err(|e| HostError::Rpc(e.to_string()))?;

        match &self.mavlink {
            // No router socket up: degrade to not_available, never error — the
            // same posture mavlink.send takes while its socket is absent.
            None => Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (Value::from("method"), Value::from("mavlink.tunnel.send")),
            ])),
            Some(client) => {
                if let Err(e) = client.send(&frame) {
                    return Ok(send_refused(e));
                }
                Ok(Value::Map(vec![
                    (Value::from("sent"), Value::Boolean(true)),
                    (
                        Value::from("payload_type"),
                        Value::Integer((payload_type as i64).into()),
                    ),
                    (
                        Value::from("payload_len"),
                        Value::Integer((payload.len() as i64).into()),
                    ),
                    (
                        Value::from("len"),
                        Value::Integer((frame.len() as i64).into()),
                    ),
                ]))
            }
        }
    }

    fn mavlink_register_component(
        &self,
        plugin_id: &str,
        args: &Value,
        granted_caps: &BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        // Order of operations: validate
        // kind, coerce component_id, then the `mavlink.component.<kind>` cap gate,
        // then the VIO-kind reservation rule, then register.
        let kind = arg_str(args, "kind").filter(|k| !k.is_empty());
        let Some(kind) = kind else {
            return Err(HostError::Rpc(
                "kind must be a non-empty string".to_string(),
            ));
        };
        let comp = arg_owned(args, "component_id");
        let comp_id = coerce_component_id(&comp).map_err(HostError::Rpc)?;
        // Required cap is decided from the requested kind, gated after validation.
        let required = format!("mavlink.component.{kind}");
        if !granted_caps.contains(&required) {
            return Err(HostError::CapabilityDenied(required));
        }
        if VIO_COMPONENT_IDS.contains(&comp_id) && kind != "vio" {
            return Err(HostError::Rpc(format!(
                "component_id {comp_id} is reserved for kind=vio"
            )));
        }
        let reg = self
            .components
            .lock()
            .expect("components mutex poisoned")
            .register(plugin_id, comp_id, kind, self.current_session(plugin_id))
            .map_err(HostError::Rpc)?;
        Ok(Value::Map(vec![
            (Value::from("registered"), Value::Boolean(true)),
            (
                Value::from("component_id"),
                Value::Integer(reg.component_id.into()),
            ),
            (Value::from("kind"), Value::from(reg.kind.as_str())),
        ]))
    }

    fn config_get(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let key = arg_str(args, "key").filter(|k| !k.is_empty());
        let Some(key) = key else {
            return Err(HostError::Rpc("key must be a non-empty string".to_string()));
        };
        let default = arg_owned(args, "default");
        let agent_id = self.agent_id_for(plugin_id);
        let value = self
            .config
            .lock()
            .expect("config mutex poisoned")
            .get(plugin_id, key, &agent_id, default);
        Ok(Value::Map(vec![(Value::from("value"), value)]))
    }

    async fn config_set(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let key = arg_str(args, "key").filter(|k| !k.is_empty());
        let Some(key) = key else {
            return Err(HostError::Rpc("key must be a non-empty string".to_string()));
        };
        if !map_has(args, "value") {
            return Err(HostError::Rpc("value missing".to_string()));
        }
        // Mirrors Python `scope = args.get("scope") or "drone"`: any falsy value
        // (nil, empty string, 0, 0.0, false, empty array/map, absent) coerces to
        // "drone". Only a truthy value that is neither drone nor global errors.
        let scope_arg = map_get(args, "scope");
        let scope = match scope_arg {
            None => "drone",
            Some(v) if !python_bool(v) => "drone",
            Some(Value::String(s)) => s.as_str().unwrap_or("drone"),
            // A truthy non-string scope (e.g. a non-empty array) is neither
            // drone nor global, so it errors with the repr of the arg.
            Some(other) => {
                return Err(HostError::Rpc(format!(
                    "scope must be drone or global, got {}",
                    py_repr_value(other)
                )))
            }
        };
        if scope != "drone" && scope != "global" {
            return Err(HostError::Rpc(format!(
                "scope must be drone or global, got {}",
                py_repr(scope)
            )));
        }
        let value = arg_owned(args, "value");
        self.store_config(plugin_id, key, value, scope)
            .await
            .map_err(HostError::Rpc)?;
        Ok(Value::Map(vec![
            (Value::from("set"), Value::Boolean(true)),
            (Value::from("scope"), Value::from(scope)),
        ]))
    }

    fn process_spawn(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let basename = arg_str(args, "basename").filter(|b| !b.is_empty());
        let Some(basename) = basename else {
            return Err(HostError::Rpc(
                "basename must be a non-empty string".to_string(),
            ));
        };
        // args defaults to []; must be a list. env defaults to {}; must be a map.
        let spawn_args = match map_get(args, "args") {
            None | Some(Value::Nil) => Value::Array(vec![]),
            Some(v @ Value::Array(_)) => v.clone(),
            Some(_) => return Err(HostError::Rpc("args must be a list of strings".to_string())),
        };
        let spawn_env = match map_get(args, "env") {
            None | Some(Value::Nil) => Value::Map(vec![]),
            Some(v @ Value::Map(_)) => v.clone(),
            Some(_) => return Err(HostError::Rpc("env must be a mapping".to_string())),
        };

        let lookup = match &self.plugin_runtime_lookup {
            None => {
                return Ok(Value::Map(vec![
                    (Value::from("error"), Value::from("not_available")),
                    (Value::from("method"), Value::from("process.spawn")),
                ]))
            }
            Some(lookup) => lookup,
        };
        // A missing registration mirrors the Python KeyError path:
        // {"error": "not_available", "method": "process.spawn",
        //  "reason": "plugin runtime not registered"}.
        let Some((install_dir, allowlist)) = lookup(plugin_id) else {
            return Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (Value::from("method"), Value::from("process.spawn")),
                (
                    Value::from("reason"),
                    Value::from("plugin runtime not registered"),
                ),
            ]));
        };

        if !allowlist.contains(basename) {
            tracing::warn!(
                plugin_id = %plugin_id,
                basename = %basename,
                allowlist_size = allowlist.len(),
                "plugin process spawn denied"
            );
            return Err(HostError::AllowlistViolation(basename.to_string()));
        }

        tracing::info!(
            plugin_id = %plugin_id,
            basename = %basename,
            "plugin process spawn authorized"
        );

        Ok(Value::Map(vec![
            (Value::from("authorized"), Value::Boolean(true)),
            (
                Value::from("install_dir"),
                Value::from(install_dir.to_string_lossy().as_ref()),
            ),
            (Value::from("basename"), Value::from(basename)),
            (Value::from("args"), spawn_args),
            (Value::from("env"), spawn_env),
        ]))
    }

    fn display_page_set(&self, plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        // Parse the request into the display-page shape, then atomically write
        // the sidecar the reserved page reads. The dispatch gate already
        // enforced the display capability before this runs.
        let page = parse_display_page(args)?;
        let rows = page.rows.len();
        let zones = page.zones.len();
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

    async fn gpio_output_set(
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

    async fn gpio_buzzer_beep(
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

    async fn video_source_set(
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

    async fn radio_aux_stream_open(
        &self,
        plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        // The dispatch gate already enforced the auxiliary-stream capability. The
        // open carries no caller-tunable parameters: the radio service resolves the
        // effective aux ports / FEC / MCS from its own config, so the host never
        // lets a plugin pick a radio-port (which could collide with the data or
        // control planes).
        //
        // The stream is one shared resource, so ownership is claimed BEFORE the
        // forward: an open while another plugin owns it is refused without
        // reaching the radio service, and two plugins opening at once cannot both
        // come away as the owner. A claim the service then refuses is released.
        let claimed = {
            let mut owner = self
                .aux_stream_owner
                .lock()
                .expect("aux stream owner mutex poisoned");
            match owner.as_deref() {
                Some(holder) if holder != plugin_id => {
                    return Err(HostError::Rpc(
                        "radio aux stream is open by another plugin".to_string(),
                    ));
                }
                Some(_) => false,
                None => {
                    *owner = Some(plugin_id.to_string());
                    true
                }
            }
        };
        let req = serde_json::json!({"op": "open"});
        let (reply, ok) = self.forward_radio_aux(req, "radio.aux_stream.open").await;
        if !ok && claimed {
            let mut owner = self
                .aux_stream_owner
                .lock()
                .expect("aux stream owner mutex poisoned");
            if owner.as_deref() == Some(plugin_id) {
                *owner = None;
            }
        }
        Ok(reply)
    }

    async fn radio_aux_stream_close(
        &self,
        plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        // The dispatch gate already enforced the auxiliary-stream capability. Only
        // the owning plugin may close: the pair is shared with the agent's own aux
        // users, so a close from anyone else is refused before it reaches the
        // radio service. A confirmed close clears the ownership record, so a later
        // disconnect does not forward a redundant close.
        self.require_aux_owner(plugin_id)?;
        let req = serde_json::json!({"op": "close"});
        let (reply, ok) = self.forward_radio_aux(req, "radio.aux_stream.close").await;
        if ok {
            let mut owner = self
                .aux_stream_owner
                .lock()
                .expect("aux stream owner mutex poisoned");
            if owner.as_deref() == Some(plugin_id) {
                *owner = None;
            }
        }
        Ok(reply)
    }

    async fn offload_advertise(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.advertise_offload(args)
    }

    async fn cloud_publish(
        &self,
        plugin_id: &str,
        args: &Value,
        granted_caps: &BTreeSet<String>,
    ) -> Result<HostResult, HostError> {
        self.publish_to_cloud(plugin_id, args, granted_caps).await
    }

    async fn cloud_records_put(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        self.put_cloud_record(plugin_id, args).await
    }

    async fn radio_aux_stream_send(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        // The dispatch gate already enforced the auxiliary-stream capability. A
        // send carries an aux channel (AppStream 8 or AppCommand 9) plus the
        // application payload. The host validates the channel onto the two
        // reserved application channels, frames the payload as an aux frame
        // (the radio service forwards opaque app datagrams), and one-shots the
        // datagram to the radio service's command socket. Only the plugin that
        // opened the stream may transmit on it.
        let channel = map_get(args, "channel")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| HostError::Rpc("channel missing or not an integer".to_string()))?;
        // Checked on the full integer: narrowing first would wrap 264 onto 8.
        let channel = match channel {
            8 => 8u8,
            9 => 9u8,
            other => return Err(HostError::Rpc(format!("unsupported aux channel {other}"))),
        };
        let payload = map_get(args, "payload")
            .map(|v| coerce_msg_bytes(v).map_err(|_| "payload must be bytes".to_string()))
            .transpose()
            .map_err(HostError::Rpc)?
            .ok_or_else(|| HostError::Rpc("payload missing".to_string()))?;
        let aux_channel = ados_protocol::aux_mux::AuxChannel::from_u8(channel)
            .expect("channel validated to 8/9 above");
        let frame = ados_protocol::aux_mux::encode(aux_channel, &payload)
            .ok_or_else(|| HostError::Rpc("payload exceeds the aux frame maximum".to_string()))?;
        self.require_aux_owner(plugin_id)?;
        let req = serde_json::json!({"op": "send", "frame": frame});
        let (reply, _ok) = self.forward_radio_aux(req, "radio.aux_stream.send").await;
        Ok(reply)
    }

    fn radio_aux_stream_subscribe_stream(
        &self,
        _plugin_id: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<(u8, Vec<u8>)>> {
        // One connection to the radio service's aux command socket for the whole
        // host process, shared by every subscriber — mirrors the BUTTON_CLIENT
        // single-connection philosophy. Started lazily (per-instance, so a test
        // with an overridden `radio_aux_cmd_path` round-trips against its stub).
        //
        // Always `Some`: an absent aux pair (or a down radio service) is a normal
        // resting state, so the subscription succeeds and stays quiet while the
        // reader retries underneath. Returning `None` would make every plugin
        // treat "no app frames yet" as an error.
        let tx = self.aux_reader.get_or_init(|| {
            let (tx, _rx) = tokio::sync::broadcast::channel(AUX_BROADCAST_DEPTH);
            let path = self.radio_aux_cmd_path.clone();
            let task_tx = tx.clone();
            tokio::spawn(async move {
                crate::button_client::reconnect_forever("radio-aux", || {
                    aux_subscribe_pump(&path, &task_tx)
                })
                .await;
            });
            tx
        });
        Some(tx.subscribe())
    }

    fn guided_setpoint_send(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        // Parse + validate the request into a setpoint, build the single
        // SET_POSITION_TARGET frame, and write it to the MAVLink socket. The
        // dispatch gate already enforced the guided-setpoint capability before
        // this runs. This is a single-shot send: the host owns no flight mode or
        // schedule, so a caller holding a velocity must re-send above the
        // autopilot's setpoint timeout (it brakes a few seconds after the last
        // setpoint) and must itself have the vehicle in its guided mode.
        let setpoint = parse_guided_setpoint(args)?;

        // Target: explicit args, else the autopilot observed on the router link.
        let (target_system, target_component) = command_target(args, self.fc_identity.get())?;

        // Build the typed message (re-validates inside) and serialize it to a v2
        // frame stamped with the companion source identity, so the bytes are
        // wire-identical to a SET_POSITION_TARGET any other agent surface emits.
        let msg = setpoint
            .build_message(target_system, target_component)
            .map_err(|e| HostError::Rpc(e.to_string()))?;
        let header = ados_protocol::mavlink::MavHeader {
            system_id: GUIDED_SOURCE_SYSTEM_ID,
            component_id: GUIDED_SOURCE_COMPONENT_ID,
            // Fire-and-forget; the router does not require a specific sequence on
            // a client-written frame (it stamps its own on its own send path).
            sequence: 0,
        };
        let frame = ados_protocol::mavlink::serialize_v2(header, &msg)
            .map_err(|e| HostError::Rpc(format!("setpoint frame encode failed: {e}")))?;

        match &self.mavlink {
            // No router socket up: degrade to not_available, never error — the
            // same posture mavlink.send takes while its socket is absent.
            None => Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (
                    Value::from("method"),
                    Value::from("flight.guided_setpoint.send"),
                ),
            ])),
            Some(client) => {
                if let Err(e) = client.send(&frame) {
                    return Ok(send_refused(e));
                }
                Ok(Value::Map(vec![
                    (Value::from("sent"), Value::Boolean(true)),
                    (
                        Value::from("msg_id"),
                        Value::Integer((setpoint_msg_id(&setpoint) as i64).into()),
                    ),
                    (
                        Value::from("len"),
                        Value::Integer((frame.len() as i64).into()),
                    ),
                ]))
            }
        }
    }

    fn rate_setpoint_send(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        // Parse + validate into an attitude setpoint, build the single
        // SET_ATTITUDE_TARGET frame, and write it to the MAVLink socket. The
        // dispatch gate already enforced the attitude/rate-setpoint capability
        // before this runs. Single-shot like the guided sender: the host owns no
        // flight mode or schedule, so a caller holding an attitude must re-send
        // above the autopilot's setpoint timeout and must itself have the vehicle
        // in a mode that accepts offboard attitude (e.g. GUIDED).
        let setpoint = parse_rate_setpoint(args)?;

        // Target: explicit args, else the autopilot observed on the router link.
        let (target_system, target_component) = command_target(args, self.fc_identity.get())?;

        // Build the typed message (re-validates inside) and serialize it to a v2
        // frame stamped with the companion source identity, so the bytes are
        // wire-identical to a SET_ATTITUDE_TARGET any other agent surface emits.
        let msg = setpoint
            .build_message(target_system, target_component)
            .map_err(|e| HostError::Rpc(e.to_string()))?;
        let header = ados_protocol::mavlink::MavHeader {
            system_id: GUIDED_SOURCE_SYSTEM_ID,
            component_id: GUIDED_SOURCE_COMPONENT_ID,
            sequence: 0,
        };
        let frame = ados_protocol::mavlink::serialize_v2(header, &msg)
            .map_err(|e| HostError::Rpc(format!("setpoint frame encode failed: {e}")))?;

        match &self.mavlink {
            None => Ok(Value::Map(vec![
                (Value::from("error"), Value::from("not_available")),
                (
                    Value::from("method"),
                    Value::from("flight.rate_setpoint.send"),
                ),
            ])),
            Some(client) => {
                if let Err(e) = client.send(&frame) {
                    return Ok(send_refused(e));
                }
                Ok(Value::Map(vec![
                    (Value::from("sent"), Value::Boolean(true)),
                    (
                        Value::from("msg_id"),
                        Value::Integer(
                            (ados_protocol::mavlink::MSG_ID_SET_ATTITUDE_TARGET as i64).into(),
                        ),
                    ),
                    (
                        Value::from("len"),
                        Value::Integer((frame.len() as i64).into()),
                    ),
                ]))
            }
        }
    }

    fn begin_session(&self, plugin_id: &str) -> u64 {
        let session = self.session_seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.sessions
            .lock()
            .expect("sessions mutex poisoned")
            .insert(plugin_id.to_string(), session);
        session
    }

    async fn release_session(&self, plugin_id: &str, session: u64) {
        // Component reservations carry the session that made them, so only
        // this session's are dropped; one a reconnect renewed stays. The
        // config store is deliberately NOT cleared (it persists across
        // reconnects).
        self.components
            .lock()
            .expect("components mutex poisoned")
            .release_session(plugin_id, session);
        // The aux stream and offload sessions are released only when no newer
        // session of this plugin has begun: a reconnect that overlaps this
        // teardown keeps what it is using.
        if self.current_session(plugin_id) != session {
            return;
        }
        // SAFE-by-default: a radio auxiliary stream never outlives the plugin that
        // opened it. If this plugin held the stream open, forward a close so the
        // additive radio pair is torn down on disconnect (it never touches the
        // data / control planes). Take the owner slot first so the forward happens
        // without holding the lock, and only when this plugin is the owner.
        let owned = {
            let mut owner = self
                .aux_stream_owner
                .lock()
                .expect("aux stream owner mutex poisoned");
            if owner.as_deref() == Some(plugin_id) {
                *owner = None;
                true
            } else {
                false
            }
        };
        // SAFE-by-default: a streaming offload session never outlives the plugin
        // that opened it. Cancel + reap every session this plugin opened on
        // disconnect. Take the handles under the lock, then cancel outside it.
        let owned_streams: Vec<OffloadStreamHandle> = {
            let mut streams = self
                .offload_streams
                .lock()
                .expect("offload streams mutex poisoned");
            let ids: Vec<String> = streams
                .iter()
                .filter(|(_, h)| h.plugin_id == plugin_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| streams.remove(&id))
                .collect()
        };
        for h in owned_streams {
            h.cancel.notify_waiters();
            h.task.abort();
        }
        if owned {
            let req = serde_json::json!({"op": "close"});
            let _ = self.forward_radio_aux(req, "radio.aux_stream.close").await;
        }
    }

    fn mavlink_subscribe_stream(
        &self,
        _plugin_id: &str,
        _msg_name: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        self.mavlink.as_ref().map(|c| c.subscribe())
    }

    fn msp_subscribe_stream(&self, _plugin_id: &str) -> Option<broadcast::Receiver<Vec<u8>>> {
        // The router fans every FC->host MSP chunk out on one broadcast; there is
        // no per-message filter (MSP has no topic). When the MSP socket is not up
        // the slot is None and no stream arms, matching the MAVLink posture.
        self.msp.as_ref().map(|c| c.subscribe())
    }

    fn vision_subscribe_stream(
        &self,
        _plugin_id: &str,
        _camera_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        // The engine fans every camera's descriptors out on one broadcast; the
        // per-camera filter is applied plugin-side (the SDK subscribe_frames
        // callback drops a non-matching camera). When the engine socket is not
        // up the slot is None and no stream arms, matching the MAVLink posture.
        //
        // The engine pushes `vision.deliver` ONLY to a connection that asked for
        // it, so handing back a receiver is not enough: the upstream
        // subscription has to be armed too, or the fanout stays permanently
        // empty and every subscribing plugin sees silence with no error
        // anywhere. Armed once per process, lazily, so a node whose plugins
        // never ask for frames never pays for the push.
        let client = self.vision.as_ref()?.clone();
        let rx = client.subscribe_frames();
        tokio::spawn(async move { client.arm_frame_push().await });
        Some(rx)
    }

    fn vision_subscribe_detection_stream(
        &self,
        _plugin_id: &str,
        _camera_id: &str,
    ) -> Option<broadcast::Receiver<Vec<u8>>> {
        // The engine fans every camera's detection batches out on one broadcast;
        // the per-camera filter is applied plugin-side (the SDK callback drops a
        // non-matching camera). When the engine socket is not up the slot is None
        // and no stream arms, matching the frame-stream posture.
        //
        // Same upstream-arming rule as the frame stream above.
        let client = self.vision.as_ref()?.clone();
        let rx = client.subscribe_detections();
        tokio::spawn(async move { client.arm_detection_push().await });
        Some(rx)
    }

    fn button_subscribe_stream(&self, _plugin_id: &str) -> Option<broadcast::Receiver<Vec<u8>>> {
        // One connection to the bus for the whole host process, shared by every
        // subscriber — the bus is a single socket and N plugins do not need N
        // connections to it. Started on first use rather than at construction so
        // a node whose plugins never ask for buttons never opens it.
        //
        // Always `Some`: an absent or down bus is a normal resting state (a
        // drone has no front panel), so the subscription succeeds and stays
        // quiet while the reader retries underneath. Returning `None` would make
        // every plugin treat "no buttons on this board" as an error to handle.
        Some(
            BUTTON_CLIENT
                .get_or_init(|| {
                    crate::button_client::ButtonClient::spawn(std::path::PathBuf::from(
                        crate::button_client::BUTTONS_SOCK,
                    ))
                })
                .subscribe(),
        )
    }

    fn display_zone_tap_stream(&self, _plugin_id: &str) -> Option<broadcast::Receiver<Vec<u8>>> {
        // One poller for the whole host process watches the display-tap sidecar,
        // shared by every subscriber — mirrors BUTTON_CLIENT's single-connection
        // philosophy. Started on first use rather than at construction so a node
        // whose plugins never subscribe to taps never starts it.
        //
        // Always `Some`: an absent display is a normal resting state (a drone has
        // no front panel), so the subscription succeeds and stays quiet while the
        // poller watches an empty file path.
        Some(
            DISPLAY_TAP_CLIENT
                .get_or_init(DisplayTapWatcher::spawn)
                .subscribe(),
        )
    }

    async fn vision_register_model(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented("vision.register_model"));
        };
        // A transport / engine error surfaces as the response envelope `error`
        // (a soft failure), exactly like the engine's own reply error would.
        client
            .register_model(args)
            .await
            .map_err(|e| HostError::Rpc(e.0))
    }

    async fn vision_read_model(
        &self,
        plugin_id: &str,
        _args: &Value,
    ) -> Result<HostResult, HostError> {
        // The model-delivery last mile: return the CALLING plugin's resolved
        // model status off the install record, so it can find where its
        // delivered model was cached. Read per call so an operator sideload that
        // flips `needs_model`→`resolved` is picked up without a restart. Unlike
        // the other vision methods this touches no engine — a missing state file
        // or an unresolved plugin yields `{models: []}`, never an error.
        let installs = crate::state::load_state(Some(&self.state_path));
        let models = crate::state::find_install(&installs, plugin_id)
            .and_then(|i| i.model_status.clone())
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        Ok(json_to_mpv(&serde_json::json!({ "models": models })))
    }

    async fn vision_infer(&self, _plugin_id: &str, args: &Value) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented("vision.infer"));
        };
        client.infer(args).await.map_err(|e| HostError::Rpc(e.0))
    }

    async fn vision_publish_detection(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented("vision.publish_detection"));
        };
        client
            .publish_detection(args)
            .await
            .map_err(|e| HostError::Rpc(e.0))
    }

    async fn vision_designate_track(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented("vision.designate_track"));
        };
        client
            .designate_track(args)
            .await
            .map_err(|e| HostError::Rpc(e.0))
    }

    async fn compute_dataset_write(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.compute.as_ref() else {
            return Ok(not_implemented("compute.dataset.write"));
        };
        let kind =
            arg_str(args, "kind").ok_or_else(|| HostError::Rpc("kind is required".to_string()))?;
        let meta = compute_json_arg(args, "meta");
        let dataset = client
            .write_dataset(kind, meta)
            .await
            .map_err(|e| HostError::Rpc(e.to_string()))?;
        compute_reply(&dataset)
    }

    async fn compute_job_submit(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.compute.as_ref() else {
            return Ok(not_implemented("compute.job.submit"));
        };
        let kind = match arg_str(args, "kind") {
            Some("reconstruct") => ComputeJobKind::Reconstruct,
            Some("perception_offload") => ComputeJobKind::PerceptionOffload,
            Some("slam_offload") => ComputeJobKind::SlamOffload,
            other => {
                return Err(HostError::Rpc(format!(
                    "unknown job kind {:?}",
                    other.unwrap_or("")
                )))
            }
        };
        let dataset_id = arg_str(args, "dataset_id").map(str::to_string);
        let params = compute_json_arg(args, "params");
        let resp = client
            .submit_job(kind, dataset_id, params, None)
            .await
            .map_err(|e| HostError::Rpc(e.to_string()))?;
        compute_reply(&resp)
    }

    async fn compute_job_read(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.compute.as_ref() else {
            return Ok(not_implemented("compute.job.read"));
        };
        let job_id = arg_str(args, "job_id")
            .ok_or_else(|| HostError::Rpc("job_id is required".to_string()))?;
        let job = client
            .job_status(job_id)
            .await
            .map_err(|e| HostError::Rpc(e.to_string()))?;
        compute_reply(&job)
    }

    async fn compute_job_outputs(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.compute.as_ref() else {
            return Ok(not_implemented("compute.job.outputs"));
        };
        let job_id = arg_str(args, "job_id")
            .ok_or_else(|| HostError::Rpc("job_id is required".to_string()))?;
        let outputs = client
            .job_outputs(job_id)
            .await
            .map_err(|e| HostError::Rpc(e.to_string()))?;
        Ok(json_to_mpv(&serde_json::json!({ "outputs": outputs })))
    }

    async fn compute_job_cancel(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(client) = self.compute.as_ref() else {
            return Ok(not_implemented("compute.job.cancel"));
        };
        let job_id = arg_str(args, "job_id")
            .ok_or_else(|| HostError::Rpc("job_id is required".to_string()))?;
        let cancelled = client
            .cancel_job(job_id)
            .await
            .map_err(|e| HostError::Rpc(e.to_string()))?;
        Ok(json_to_mpv(&serde_json::json!({ "cancelled": cancelled })))
    }

    async fn compute_stream_open(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        // The plugin names the camera + model + execution intent; the host owns
        // the source (the drone's LAN-reachable RTSP feed) and the node reach
        // (from the offload-link sidecar), so a sandboxed plugin can neither point
        // the node at an arbitrary URL nor learn the pairing key.
        let camera_id = arg_str(args, "camera_id").unwrap_or("front").to_string();
        let execution = arg_str(args, "execution").unwrap_or("auto");
        let (width, height, target_budget_ms) = offload_frame_and_budget(args);
        let model_id = arg_str(args, "model_id").map(str::to_string);

        let session_id = match arg_str(args, "session_id") {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                let n = self.offload_session_seq.fetch_add(1, Ordering::Relaxed);
                format!("plugin-{plugin_id}-{n}")
            }
        };

        // Resolve the perception tier from the offload-link sidecar — the same
        // signal `ados_offload::pick_tier` / the status surfaces read, not a
        // second copy of the tier logic.
        let link = read_offload_link_from(&self.offload_link_path, now_epoch_ms());
        let offload_path = link.as_ref().is_some_and(|l| l.is_offload_path());

        // Decide local vs offload per the requested execution + the tier signal.
        // `auto` (and any unrecognised value, which defaults to auto) offloads
        // only when the sidecar reports a paired node on an acceptable bearer.
        let go_offload = match execution {
            "local" => false,
            "offload" => true,
            _ => offload_path,
        };

        if !go_offload {
            // LOCAL: the plugin runs its model on the local accelerator (the
            // existing vision path); no offload session is started here.
            return Ok(json_to_mpv(&serde_json::json!({
                "execution": "local",
                "opened": false,
                "session_id": session_id,
                "camera_id": camera_id,
            })));
        }

        // OFFLOAD: the node the sidecar named, or refuse honestly (never a
        // fabricated node). Forced offload with no paired node in the
        // sidecar has no target (the drone is not currently offloading).
        let target = link
            .as_ref()
            .and_then(|l| l.target.clone())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                HostError::Rpc(
                    "no paired compute node to offload to (the drone is not currently offloading to a workstation)"
                        .to_string(),
                )
            })?;
        let base_url = format!("http://{target}");
        let credential = workstation_credential(
            &self.workstation_credentials_path,
            link.as_ref().and_then(|l| l.device_id.as_deref()),
        );

        // The node pulls the drone's RTSP feed on the drone's LAN-reachable egress
        // IP toward the node (never localhost — the node is a different machine).
        let local_ip = local_ip_towards(&target).await.ok_or_else(|| {
            HostError::Rpc(
                "cannot determine a LAN-reachable camera address toward the compute node"
                    .to_string(),
            )
        })?;
        let rtsp_url = format!("rtsp://{local_ip}:{OFFLOAD_RTSP_PORT}/{OFFLOAD_RTSP_PATH}");

        // Check the registry and spawn under ONE lock hold, so two opens of the
        // same id cannot both spawn a lane (the loser's lane would run with no
        // handle to close it).
        let mut streams = self
            .offload_streams
            .lock()
            .expect("offload streams mutex poisoned");
        match streams.get(&session_id) {
            // Another plugin's session: refuse rather than report it open to a
            // caller that could neither close it nor read its health.
            Some(h) if h.plugin_id != plugin_id => {
                return Err(HostError::Rpc(format!(
                    "offload session {session_id} is held by another plugin"
                )));
            }
            // Idempotent re-open: this plugin's session is still running, a
            // no-op success mirroring the node-side registry dedup.
            Some(h) if !h.task.is_finished() => {
                return Ok(json_to_mpv(&serde_json::json!({
                    "execution": "offload",
                    "opened": true,
                    "already_open": true,
                    "session_id": session_id,
                    "camera_id": camera_id,
                    "source": rtsp_url,
                    "node": base_url,
                })));
            }
            // This plugin's session, but its lane is gone: reap it and start a
            // fresh one below instead of reporting a dead session as open.
            Some(_) => {
                streams.remove(&session_id);
            }
            None => {}
        }

        // Spawn the supervised lane: submit the streaming-session job to the
        // node, subscribe to the node's per-session detection WS, and republish
        // each returned batch onto the drone's local `vision.detection` bus
        // through the offload safety gate, restarting whenever it ends until the
        // session is closed. This is the same `run_offload_orchestrator` the
        // auto-offload reconciler drives, so the plugin's detections are
        // execution-transparent on the bus.
        let cancel = Arc::new(Notify::new());
        let task = tokio::spawn(supervise_offload_lane(
            OffloadLane {
                session_id: session_id.clone(),
                camera_id: camera_id.clone(),
                rtsp_url: rtsp_url.clone(),
                width,
                height,
                target_budget_ms,
                model_id,
                base_url: base_url.clone(),
                credential: credential.clone(),
            },
            cancel.clone(),
        ));
        streams.insert(
            session_id.clone(),
            OffloadStreamHandle {
                plugin_id: plugin_id.to_string(),
                cancel,
                task,
                node_base_url: base_url.clone(),
                credential,
            },
        );
        drop(streams);

        tracing::info!(plugin_id, session = %session_id, node = %base_url, "plugin opened an offload stream");
        Ok(json_to_mpv(&serde_json::json!({
            "execution": "offload",
            "opened": true,
            "session_id": session_id,
            "camera_id": camera_id,
            "source": rtsp_url,
            "node": base_url,
        })))
    }

    async fn compute_stream_close(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let session_id = arg_str(args, "session_id")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| HostError::Rpc("session_id is required".to_string()))?;
        // Only the plugin that opened a session may close it. Remove it from the
        // map under the lock, then cancel + reap outside the lock.
        let handle = {
            let mut streams = self
                .offload_streams
                .lock()
                .expect("offload streams mutex poisoned");
            match streams.get(session_id) {
                Some(h) if h.plugin_id == plugin_id => streams.remove(session_id),
                _ => None,
            }
        };
        let closed = match handle {
            Some(h) => {
                h.cancel.notify_waiters();
                h.task.abort();
                true
            }
            None => false,
        };
        Ok(json_to_mpv(
            &serde_json::json!({ "closed": closed, "session_id": session_id }),
        ))
    }

    async fn compute_stream_health(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let session_id = arg_str(args, "session_id")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| HostError::Rpc("session_id is required".to_string()))?;
        // The node reach for THIS session (opened by this plugin). No handle ⇒ the
        // session is not open here ⇒ report closed (never a fabricated live state).
        let reach = {
            let streams = self
                .offload_streams
                .lock()
                .expect("offload streams mutex poisoned");
            streams
                .get(session_id)
                .filter(|h| h.plugin_id == plugin_id)
                .map(|h| (h.node_base_url.clone(), h.credential.clone()))
        };
        let Some((base_url, credential)) = reach else {
            return Ok(json_to_mpv(&serde_json::json!({
                "session_id": session_id,
                "state": "closed",
                "found": false,
            })));
        };
        // Read the node's live session registry and find this session. A session
        // absent from the node's list has been reaped ⇒ closed.
        let client = ComputeClient::new(base_url, credential);
        let sessions = client
            .sessions()
            .await
            .map_err(|e| HostError::Rpc(format!("read node sessions: {e}")))?;
        match sessions.into_iter().find(|s| s.session.id == session_id) {
            Some(view) => {
                let mut json =
                    serde_json::to_value(&view).map_err(|e| HostError::Rpc(e.to_string()))?;
                if let serde_json::Value::Object(ref mut m) = json {
                    m.insert("found".to_string(), serde_json::Value::Bool(true));
                }
                Ok(json_to_mpv(&json))
            }
            None => Ok(json_to_mpv(&serde_json::json!({
                "session_id": session_id,
                "state": "closed",
                "found": false,
            }))),
        }
    }
}
