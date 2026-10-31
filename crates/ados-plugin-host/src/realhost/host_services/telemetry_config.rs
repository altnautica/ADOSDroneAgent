//! Telemetry, config and process host methods.

use super::*;

impl RealHost {
    pub(super) fn serve_telemetry_extend(
        &self,
        _plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let channel = arg_str(args, "channel").filter(|c| !c.is_empty());
        let Some(channel) = channel else {
            return Err(HostError::Rpc(
                "channel must be a non-empty string".to_string(),
            ));
        };
        // payload defaults to an empty map; a non-map payload is an RPC error.
        let payload = match map_get(args, "payload") {
            None | Some(Value::Nil) => Value::Map(vec![]),
            Some(v @ Value::Map(_)) => v.clone(),
            Some(_) => return Err(HostError::Rpc("payload must be a mapping".to_string())),
        };
        // The server mirrors the merged channel into the plugin's state sidecar
        // (what the GCS reads); the validated payload is echoed back.
        Ok(Value::Map(vec![
            (Value::from("merged"), Value::Boolean(true)),
            (Value::from("channel"), Value::from(channel)),
            (Value::from("payload"), payload),
        ]))
    }

    pub(super) fn serve_telemetry_state_stream(
        &self,
        _plugin_id: &str,
    ) -> Option<broadcast::Receiver<Arc<Value>>> {
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

    pub(super) fn serve_config_get(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
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

    pub(super) async fn serve_config_set(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let key = arg_str(args, "key").filter(|k| !k.is_empty());
        let Some(key) = key else {
            return Err(HostError::Rpc("key must be a non-empty string".to_string()));
        };
        if !map_has(args, "value") {
            return Err(HostError::Rpc("value missing".to_string()));
        }
        // Any falsy scope (nil, empty string, 0, 0.0, false, empty array/map,
        // absent) coerces to "drone". Only a truthy value that is neither drone
        // nor global errors.
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

    pub(super) fn serve_process_spawn(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
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
        // A plugin with no registered runtime answers not_available with the
        // reason, rather than an error.
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
}
