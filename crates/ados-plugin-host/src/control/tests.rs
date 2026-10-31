use super::*;
use std::sync::Mutex;

/// A stub host recording the last applied write, with a configurable result.
#[derive(Default)]
struct StubHost {
    last: Mutex<Option<(String, String, Value, String)>>,
    fail: Option<String>,
}

impl ConfigControl for StubHost {
    async fn apply_config_set(
        &self,
        plugin_id: &str,
        key: &str,
        value: Value,
        scope: &str,
    ) -> Result<String, String> {
        if let Some(err) = &self.fail {
            return Err(err.clone());
        }
        *self.last.lock().unwrap() = Some((
            plugin_id.to_string(),
            key.to_string(),
            value,
            scope.to_string(),
        ));
        // drone with an empty agent degrades to global in the real store;
        // the stub just echoes the requested scope.
        Ok(scope.to_string())
    }

    fn config_snapshot(&self, plugin_id: &str) -> Result<Value, String> {
        if let Some(err) = &self.fail {
            return Err(err.clone());
        }
        let last = self.last.lock().unwrap().clone();
        Ok(Value::Map(match last {
            Some((p, k, v, _)) if p == plugin_id => vec![(Value::from(k), v)],
            _ => vec![],
        }))
    }
}

fn request(args: Vec<(Value, Value)>) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "request".to_string(),
        method: METHOD_CONFIG_SET.to_string(),
        capability: String::new(),
        args: Value::Map(args),
        request_id: "ctl-test".to_string(),
        token: String::new(),
        error: None,
    }
}

#[tokio::test]
async fn applies_a_config_write_and_echoes_scope() {
    let host = StubHost::default();
    let req = request(vec![
        (
            Value::from("plugin_id"),
            Value::from("com.altnautica.follow-me"),
        ),
        (Value::from("key"), Value::from("active")),
        (Value::from("value"), Value::Boolean(true)),
        (Value::from("scope"), Value::from("drone")),
    ]);
    let resp = handle_request(&host, &req).await;
    assert_eq!(resp.error, None);
    let last = host.last.lock().unwrap().clone().expect("a write");
    assert_eq!(last.0, "com.altnautica.follow-me");
    assert_eq!(last.1, "active");
    assert_eq!(last.2, Value::Boolean(true));
    assert_eq!(last.3, "drone");
}

#[tokio::test]
async fn defaults_scope_to_drone_when_absent() {
    let host = StubHost::default();
    let req = request(vec![
        (Value::from("plugin_id"), Value::from("p")),
        (Value::from("key"), Value::from("k")),
        (Value::from("value"), Value::from(3)),
    ]);
    let resp = handle_request(&host, &req).await;
    assert_eq!(resp.error, None);
    assert_eq!(host.last.lock().unwrap().clone().unwrap().3, "drone");
}

#[tokio::test]
async fn rejects_a_missing_plugin_id() {
    let host = StubHost::default();
    let req = request(vec![
        (Value::from("key"), Value::from("k")),
        (Value::from("value"), Value::from(1)),
    ]);
    let resp = handle_request(&host, &req).await;
    assert!(resp.error.unwrap().contains("plugin_id"));
    assert!(host.last.lock().unwrap().is_none());
}

#[tokio::test]
async fn rejects_a_missing_value() {
    let host = StubHost::default();
    let req = request(vec![
        (Value::from("plugin_id"), Value::from("p")),
        (Value::from("key"), Value::from("k")),
    ]);
    let resp = handle_request(&host, &req).await;
    assert!(resp.error.unwrap().contains("value"));
}

#[tokio::test]
async fn surfaces_a_host_error() {
    let host = StubHost {
        fail: Some("scope must be drone or global, got nonsense".to_string()),
        ..StubHost::default()
    };
    let req = request(vec![
        (Value::from("plugin_id"), Value::from("p")),
        (Value::from("key"), Value::from("k")),
        (Value::from("value"), Value::from(1)),
        (Value::from("scope"), Value::from("nonsense")),
    ]);
    let resp = handle_request(&host, &req).await;
    assert!(resp.error.unwrap().contains("scope must be"));
}

#[tokio::test]
async fn rejects_an_unknown_method() {
    let host = StubHost::default();
    let mut req = request(vec![]);
    req.method = "config.delete".to_string();
    let resp = handle_request(&host, &req).await;
    assert!(resp.error.unwrap().contains("unknown control method"));
}

/// End-to-end over a real bound socket: a client envelope round-trips and the
/// write lands on the host.
#[tokio::test]
async fn round_trips_over_a_bound_socket() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(StubHost::default());
    let invoke = Arc::new(InvokeRegistry::new());
    let (path, task) = serve_control(host.clone(), invoke, None, dir.path().to_path_buf()).unwrap();

    let req = request(vec![
        (Value::from("plugin_id"), Value::from("p")),
        (Value::from("key"), Value::from("active")),
        (Value::from("value"), Value::Boolean(true)),
    ]);
    let mut stream = UnixStream::connect(&path).await.unwrap();
    stream
        .write_all(&req.encode_frame().unwrap())
        .await
        .unwrap();
    stream.flush().await.unwrap();

    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await.unwrap();
    let len = decode_len(header, PLUGIN_MAX_FRAME, false).unwrap();
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.unwrap();
    let resp = Envelope::from_msgpack(&body).unwrap();
    assert_eq!(resp.error, None);
    assert_eq!(host.last.lock().unwrap().clone().unwrap().1, "active");

    task.abort();
}

/// The control socket is reachable by root only: it lives in its own `0700`
/// dir outside the per-plugin socket dir the plugin units can write, the
/// socket itself is `0600`, and the accept policy refuses a plugin process.
#[tokio::test]
async fn the_control_socket_is_root_only_and_refuses_a_plugin_peer() {
    let dir = tempfile::tempdir().unwrap();
    let control_dir = dir.path().join("plugin-host");
    let host = Arc::new(StubHost::default());
    let invoke = Arc::new(InvokeRegistry::new());
    let (path, task) = serve_control(host, invoke, None, control_dir.clone()).unwrap();

    assert_eq!(path.parent(), Some(control_dir.as_path()));
    let dir_mode = std::fs::metadata(&control_dir)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700, "control dir must be owner-only");
    let sock_mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(
        sock_mode & 0o777,
        0o600,
        "control socket must be owner-only"
    );
    assert!(
        !Path::new(DEFAULT_CONTROL_DIR).starts_with(crate::server::DEFAULT_SOCKET_DIR),
        "the control dir must not sit under the per-plugin socket dir"
    );

    // A plugin process: uid 1000 with only its own group, against a root
    // daemon on a host that has the operator group.
    assert!(!ados_protocol::ipc::operator_peer_allowed(
        1000,
        &[1000],
        0,
        Some(990)
    ));
    // An operator-group member is admitted by the same policy.
    assert!(ados_protocol::ipc::operator_peer_allowed(
        1000,
        &[1000, 990],
        0,
        Some(990)
    ));

    task.abort();
}

fn tool_request(plugin_id: &str, tool: &str) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "request".to_string(),
        method: METHOD_TOOL_INVOKE.to_string(),
        capability: String::new(),
        args: Value::Map(vec![
            (Value::from("plugin_id"), Value::from(plugin_id)),
            (Value::from("tool"), Value::from(tool)),
            (Value::from("arguments"), Value::Map(vec![])),
        ]),
        request_id: "ctl-inv".to_string(),
        token: String::new(),
        error: None,
    }
}

#[tokio::test]
async fn tool_invoke_against_no_connection_errors() {
    let invoke = InvokeRegistry::new();
    let resp = handle_tool_invoke(&invoke, &tool_request("com.x.p", "t")).await;
    assert_eq!(resp.method, METHOD_TOOL_INVOKE);
    assert!(resp.error.unwrap().contains("plugin_not_running"));
}

#[tokio::test]
async fn tool_invoke_routes_to_a_registered_connection() {
    let invoke = InvokeRegistry::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::invoke::InvokeRequest>(4);
    invoke.register("com.x.p", tx);
    // A fake connection replies with the tool name echoed.
    let responder = tokio::spawn(async move {
        let req = rx.recv().await.unwrap();
        let _ = req.reply.send(Ok(Value::Map(vec![(
            Value::from("ran"),
            Value::from(req.tool),
        )])));
    });
    let resp = handle_tool_invoke(&invoke, &tool_request("com.x.p", "greet")).await;
    assert_eq!(resp.error, None);
    assert_eq!(
        resp.args,
        Value::Map(vec![(Value::from("ran"), Value::from("greet"))])
    );
    responder.await.unwrap();
}

#[tokio::test]
async fn tool_invoke_rejects_a_missing_tool_name() {
    let invoke = InvokeRegistry::new();
    let mut req = tool_request("com.x.p", "t");
    // Drop the tool arg.
    req.args = Value::Map(vec![(Value::from("plugin_id"), Value::from("com.x.p"))]);
    let resp = handle_tool_invoke(&invoke, &req).await;
    assert!(resp.error.unwrap().contains("tool must be"));
}
