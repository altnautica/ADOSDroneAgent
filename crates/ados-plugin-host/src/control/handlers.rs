//! Request handlers for the control socket.

use super::*;

pub(super) fn ok_response(request_id: &str, scope: &str) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: METHOD_CONFIG_SET.to_string(),
        capability: String::new(),
        args: Value::Map(vec![
            (Value::from("set"), Value::Boolean(true)),
            (Value::from("scope"), Value::from(scope)),
        ]),
        request_id: request_id.to_string(),
        token: String::new(),
        error: None,
    }
}

pub(super) fn err_response(request_id: &str, message: String) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: METHOD_CONFIG_SET.to_string(),
        capability: String::new(),
        args: Value::Map(vec![]),
        request_id: request_id.to_string(),
        token: String::new(),
        error: Some(message),
    }
}

/// Answer a `config.get` request with the plugin's effective config.
pub(super) fn handle_config_get<H: ConfigControl>(host: &H, req: &Envelope) -> Envelope {
    let method = METHOD_CONFIG_GET;
    let Some(plugin_id) = arg_str(&req.args, "plugin_id").filter(|s| !s.is_empty()) else {
        return lifecycle_err(
            &req.request_id,
            method,
            "plugin_id must be a non-empty string".into(),
        );
    };
    match host.config_snapshot(plugin_id) {
        Ok(values) => Envelope {
            version: PROTOCOL_VERSION,
            kind: "response".to_string(),
            method: method.to_string(),
            capability: String::new(),
            args: Value::Map(vec![(Value::from("values"), values)]),
            request_id: req.request_id.clone(),
            token: String::new(),
            error: None,
        },
        Err(e) => lifecycle_err(&req.request_id, method, e),
    }
}

/// Handle one decoded control request against the host. Free of socket I/O so
/// it unit tests directly.
pub(super) async fn handle_request<H: ConfigControl>(host: &H, req: &Envelope) -> Envelope {
    if req.method == METHOD_CONFIG_GET {
        return handle_config_get(host, req);
    }
    if req.method != METHOD_CONFIG_SET {
        return err_response(
            &req.request_id,
            format!("unknown control method: {}", req.method),
        );
    }
    let Some(plugin_id) = arg_str(&req.args, "plugin_id").filter(|s| !s.is_empty()) else {
        return err_response(
            &req.request_id,
            "plugin_id must be a non-empty string".into(),
        );
    };
    let Some(key) = arg_str(&req.args, "key").filter(|s| !s.is_empty()) else {
        return err_response(&req.request_id, "key must be a non-empty string".into());
    };
    let Some(value) = map_get(&req.args, "value").cloned() else {
        return err_response(&req.request_id, "value missing".into());
    };
    // Scope defaults to drone (the per-drone namespace a skill toggle lives in);
    // an absent or empty scope is the common case.
    let scope = arg_str(&req.args, "scope")
        .filter(|s| !s.is_empty())
        .unwrap_or("drone");
    match host.apply_config_set(plugin_id, key, value, scope).await {
        Ok(effective) => ok_response(&req.request_id, &effective),
        Err(e) => err_response(&req.request_id, e),
    }
}

pub(super) fn tool_ok(request_id: &str, result: Value) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: METHOD_TOOL_INVOKE.to_string(),
        capability: String::new(),
        args: result,
        request_id: request_id.to_string(),
        token: String::new(),
        error: None,
    }
}

pub(super) fn tool_err(request_id: &str, message: String) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: METHOD_TOOL_INVOKE.to_string(),
        capability: String::new(),
        args: Value::Map(vec![]),
        request_id: request_id.to_string(),
        token: String::new(),
        error: Some(message),
    }
}

/// Handle one `tool.invoke` control request by routing it to the live plugin
/// connection via the invoke registry. Async because it awaits the plugin's
/// reply. `arguments` defaults to an empty map; `timeout_ms` to the registry
/// default. A not-connected / slow / erroring plugin yields an error response,
/// never a hang.
pub(super) async fn handle_tool_invoke(invoke: &InvokeRegistry, req: &Envelope) -> Envelope {
    let Some(plugin_id) = arg_str(&req.args, "plugin_id").filter(|s| !s.is_empty()) else {
        return tool_err(
            &req.request_id,
            "plugin_id must be a non-empty string".into(),
        );
    };
    let Some(tool) = arg_str(&req.args, "tool").filter(|s| !s.is_empty()) else {
        return tool_err(&req.request_id, "tool must be a non-empty string".into());
    };
    let arguments = map_get(&req.args, "arguments")
        .cloned()
        .unwrap_or(Value::Map(vec![]));
    let timeout = map_get(&req.args, "timeout_ms")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_INVOKE_TIMEOUT);
    match invoke.invoke(plugin_id, tool, arguments, timeout).await {
        Ok(result) => tool_ok(&req.request_id, result),
        Err(e) => tool_err(&req.request_id, e),
    }
}

/// Handle a lifecycle control request (`token.rotate` / `plugin.reconcile`).
///
/// A daemon with no reconciler wired answers with an explicit error naming the
/// gap, never a bare success: the caller is a lifecycle controller that has
/// just told an operator a revoke took effect, so a false ack here is the exact
/// failure the control socket exists to close.
pub(super) fn handle_lifecycle(
    lifecycle: Option<&Arc<dyn LifecycleControl>>,
    req: &Envelope,
) -> Envelope {
    let Some(lifecycle) = lifecycle else {
        return lifecycle_err(
            &req.request_id,
            &req.method,
            "plugin host has no lifecycle reconciler wired; the change applies \
             when it next reads state"
                .into(),
        );
    };
    if req.method == METHOD_PLUGIN_RECONCILE {
        let (started, stopped, serving) = lifecycle.reconcile();
        return Envelope {
            version: PROTOCOL_VERSION,
            kind: "response".to_string(),
            method: req.method.clone(),
            capability: String::new(),
            args: Value::Map(vec![
                (Value::from("started"), Value::from(started as u64)),
                (Value::from("stopped"), Value::from(stopped as u64)),
                (Value::from("serving"), Value::from(serving as u64)),
            ]),
            request_id: req.request_id.clone(),
            token: String::new(),
            error: None,
        };
    }
    let Some(plugin_id) = arg_str(&req.args, "plugin_id").filter(|s| !s.is_empty()) else {
        return lifecycle_err(
            &req.request_id,
            &req.method,
            "plugin_id must be a non-empty string".into(),
        );
    };
    if req.method == METHOD_PLUGIN_ACCOUNT {
        let ensure = match arg_str(&req.args, "op") {
            Some("ensure") => true,
            Some("remove") => false,
            _ => {
                return lifecycle_err(
                    &req.request_id,
                    &req.method,
                    "op must be \"ensure\" or \"remove\"".into(),
                )
            }
        };
        return match lifecycle.account(plugin_id, ensure) {
            Ok(()) => Envelope {
                version: PROTOCOL_VERSION,
                kind: "response".to_string(),
                method: req.method.clone(),
                capability: String::new(),
                args: Value::Map(vec![(Value::from("done"), Value::Boolean(true))]),
                request_id: req.request_id.clone(),
                token: String::new(),
                error: None,
            },
            Err(e) => lifecycle_err(&req.request_id, &req.method, e),
        };
    }
    if req.method == METHOD_PLUGIN_PURGE_DATA {
        return match lifecycle.purge_data(plugin_id) {
            Ok(()) => Envelope {
                version: PROTOCOL_VERSION,
                kind: "response".to_string(),
                method: req.method.clone(),
                capability: String::new(),
                args: Value::Map(vec![(Value::from("purged"), Value::Boolean(true))]),
                request_id: req.request_id.clone(),
                token: String::new(),
                error: None,
            },
            Err(e) => lifecycle_err(&req.request_id, &req.method, e),
        };
    }
    match lifecycle.rotate_token(plugin_id) {
        Ok(pushed) => Envelope {
            version: PROTOCOL_VERSION,
            kind: "response".to_string(),
            method: req.method.clone(),
            capability: String::new(),
            args: Value::Map(vec![
                (Value::from("rotated"), Value::Boolean(true)),
                // False means the token was written but no session was open to
                // receive it, which is correct for an enabled plugin that has
                // not connected yet. The caller reports the difference rather
                // than claiming the live plugin was updated.
                (Value::from("delivered"), Value::Boolean(pushed)),
            ]),
            request_id: req.request_id.clone(),
            token: String::new(),
            error: None,
        },
        Err(e) => lifecycle_err(&req.request_id, &req.method, e),
    }
}

pub(super) fn lifecycle_err(request_id: &str, method: &str, message: String) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: method.to_string(),
        capability: String::new(),
        args: Value::Map(vec![]),
        request_id: request_id.to_string(),
        token: String::new(),
        error: Some(message),
    }
}
