use super::*;

fn manifest(services: &str) -> PluginManifest {
    PluginManifest::from_yaml_text(&format!(
        "id: com.example.svc\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\n\
         agent:\n  entrypoint: agent/py/x.py\n  contributes:\n    services:\n{services}"
    ))
    .unwrap()
}

#[test]
fn services_parse_both_shapes_and_refuse_a_foreign_slice() {
    let m = manifest(
        "      - bridge\n      - name: api\n        command: bin/api --port 9100\n        \
         ready_check: http://127.0.0.1:9100/health\n        restart: always\n",
    );
    let s = declared_services(&m).unwrap();
    assert_eq!(s[0].name, "bridge");
    assert_eq!(s[0].command, "bridge");
    assert_eq!(s[0].restart, "on-failure");
    assert_eq!(
        s[1].ready_check,
        Some(ReadyCheck::Http {
            url: "http://127.0.0.1:9100/health".into(),
            port: 9100,
            path: "/health".into()
        })
    );
    let bad = manifest("      - name: api\n        command: x\n        slice: system.slice\n");
    assert!(declared_services(&bad).is_err());
    let off_box = manifest(
        "      - name: api\n        command: x\n        ready_check: http://10.0.0.2:80/\n",
    );
    assert!(declared_services(&off_box).is_err());
}

#[test]
fn a_service_command_cannot_smuggle_a_directive_or_a_specifier() {
    assert!(service_argv("bin/x\nExecStartPre=+/bin/sh").is_err());
    assert!(service_argv("+bin/x").is_err());
    assert!(service_argv("bin/x ; rm").is_err());
    let words: Vec<String> = service_argv("bin/x --name '%h $HOME'")
        .unwrap()
        .iter()
        .map(|w| exec_word(w))
        .collect();
    assert_eq!(words.join(" "), "bin/x --name \"%%h $$HOME\"");
}

fn render(m: &PluginManifest, granted: &[&str]) -> String {
    let granted: BTreeSet<String> = granted.iter().map(|s| s.to_string()).collect();
    let s = &declared_services(m).unwrap()[0];
    let paths = crate::supervisor::tests_support::fhs_paths();
    crate::backend::render_systemd(
        &build_service_spec(m, s, &paths, "workstation", &granted, true).unwrap(),
    )
}

#[test]
fn a_service_unit_runs_in_the_plugin_slice_under_the_plugin_sandbox() {
    let m = manifest("      - name: api\n        command: bin/api\n");
    let unit = render(&m, &[]);
    assert!(unit.contains("Slice=ados-plugins.slice"));
    assert!(unit.contains("WorkingDirectory=/var/ados/plugins/com.example.svc"));
    assert!(unit.contains("ExecStart=bin/api"));
    assert!(unit.contains("User=ados-plg-39e76452\nGroup=ados-plugins\n"));
    assert!(unit.contains("NoNewPrivileges=yes"));
    assert_eq!(
        service_unit_name_for("com.example.svc", "api"),
        "ados-plugin-com-example-svc-39e76452-api.service"
    );
}

#[test]
fn a_service_shares_the_plugins_identity_on_the_host_socket() {
    // A declared service reaches the host SDK exactly as the main process
    // does: same socket, same token file, same socket-dir bind.
    let unit = render(
        &manifest("      - name: api\n        command: bin/api\n"),
        &[],
    );
    assert!(
        unit.contains("Environment=ADOS_PLUGIN_SOCKET=/run/ados/plugins/com.example.svc/host.sock")
    );
    assert!(
        unit.contains("LoadCredential=ados-plugin-token:/run/ados/plugins/com.example.svc.token")
    );
    assert!(unit.contains("BindReadOnlyPaths=/run/ados/plugins/com.example.svc\n"));
    assert!(unit.contains("Environment=ADOS_NODE_PROFILE=workstation\n"));
}

fn listener(perms: &str) -> String {
    format!(
        "id: com.example.svc\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\n\
         agent:\n  entrypoint: agent/py/x.py\n  permissions: [{perms}]\n  contributes:\n    \
         services:\n      - name: node\n        command: bin/node\n        \
         listen_ports: [8092]\n        profiles: [workstation, ground_station]\n"
    )
}

#[test]
fn a_listener_binds_only_its_declared_ports_and_only_when_granted() {
    let m = PluginManifest::from_yaml_text(&listener("network.listen, network.outbound")).unwrap();
    let s = &declared_services(&m).unwrap()[0];
    assert_eq!(s.listen_ports, vec![8092]);
    assert_eq!(
        s.profiles,
        Some(vec![
            "workstation".to_string(),
            "ground-station".to_string()
        ])
    );
    assert!(s.applies_to("workstation") && !s.applies_to("drone"));

    let ungranted = render(&m, &["network.outbound"]);
    assert!(ungranted.contains("SocketBindDeny=any"));
    assert!(!ungranted.contains("SocketBindAllow="));
    let granted = render(&m, &["network.outbound", "network.listen"]);
    assert!(
        granted.contains("SocketBindAllow=tcp:8092\nSocketBindDeny=any"),
        "{granted}"
    );
}

#[test]
fn listen_ports_without_the_listen_and_network_permissions_are_refused() {
    for perms in ["", "network.listen", "network.outbound"] {
        let m = PluginManifest::from_yaml_text(&listener(perms)).unwrap();
        let err = declared_services(&m).unwrap_err();
        assert!(
            err.0.contains("declares listen_ports"),
            "{perms:?}: {}",
            err.0
        );
    }
    let too_low = listener("network.listen, network.outbound").replace("[8092]", "[80]");
    let err = declared_services(&PluginManifest::from_yaml_text(&too_low).unwrap()).unwrap_err();
    assert!(err.0.contains("outside 1024..=65535"), "{}", err.0);
    let too_many = listener("network.listen, network.outbound")
        .replace("[8092]", "[2001, 2002, 2003, 2004, 2005]");
    assert!(declared_services(&PluginManifest::from_yaml_text(&too_many).unwrap()).is_err());
}

#[test]
fn an_http_probe_reads_the_status_code() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        for reply in [
            "HTTP/1.1 204 No Content\r\n\r\n",
            "HTTP/1.1 503 Busy\r\n\r\n",
        ] {
            let (mut s, _) = listener.accept().unwrap();
            // Read the whole request head before answering: replying and
            // closing mid-request leaves the client writing into a closed
            // socket (EPIPE), which is not the answer under test.
            let mut head = Vec::new();
            let mut buf = [0u8; 256];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            s.write_all(reply.as_bytes()).unwrap();
        }
    });
    assert_eq!(probe_http(port, "/health", false), (true, None));
    assert_eq!(
        probe_http(port, "/health", false),
        (false, Some("http status 503".to_string()))
    );
    server.join().unwrap();
}
