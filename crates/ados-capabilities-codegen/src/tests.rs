use super::*;
use crate::contracts_emit::{emit_python_contracts, emit_rust_contracts, emit_ts_contracts};

fn sample() -> Catalog {
    Catalog {
        agent: vec![
            Cap {
                id: "event.publish".into(),
                label: "Publish".into(),
                description: "Emit events.".into(),
                category: "data_network".into(),
                risk: "low".into(),
                risk_reason: "Sandboxed.".into(),
                enforced: true,
            },
            Cap {
                id: "mavlink.read".into(),
                label: "Read MAVLink".into(),
                description: "Observe the stream.".into(),
                category: "flight_control".into(),
                risk: "low".into(),
                risk_reason: "Read-only.".into(),
                enforced: false,
            },
        ],
        gcs: vec![Cap {
            id: "ui.slot.fc-tab".into(),
            label: "FC tab".into(),
            description: "Add a tab.".into(),
            category: "ui_slot".into(),
            risk: "low".into(),
            risk_reason: "Sandboxed iframe.".into(),
            enforced: false,
        }],
        method: vec![],
    }
}

#[test]
fn rust_output_has_both_caps_and_enforced_flag() {
    let rs = emit_rust(&sample());
    assert!(rs.contains("id: \"event.publish\""));
    assert!(rs.contains("enforced: true"));
    assert!(rs.contains("id: \"mavlink.read\""));
    assert!(rs.contains("enforced: false"));
    assert!(rs.contains("pub const AGENT_CAPABILITIES"));
}

#[test]
fn python_output_has_frozenset_and_enforced_subset() {
    let py = emit_python(&sample());
    assert!(py.contains("AGENT_CAPABILITIES: frozenset[str]"));
    assert!(py.contains("\"event.publish\","));
    // Only the enforced cap appears in the enforced subset.
    let enforced_block = py
        .split("ENFORCED_AGENT_CAPABILITIES")
        .nth(1)
        .unwrap()
        .split("CAPABILITY_CATALOG")
        .next()
        .unwrap();
    assert!(enforced_block.contains("event.publish"));
    assert!(!enforced_block.contains("mavlink.read"));
}

#[test]
fn ts_gcs_output_has_id_list_and_catalog() {
    let ts = emit_ts_catalog(
        &sample().gcs,
        "GCS_CAPABILITY_CATALOG",
        Some("GCS_CAPABILITIES"),
    );
    assert!(ts.contains("export const GCS_CAPABILITIES = ["));
    assert!(ts.contains("\"ui.slot.fc-tab\","));
    assert!(ts.contains("export const GCS_CAPABILITY_CATALOG: Record<string, CapabilityMeta>"));
}

#[test]
fn escaping_handles_quotes_and_backslashes() {
    assert_eq!(esc(r#"a "b" \c"#), r#"a \"b\" \\c"#);
}

#[test]
fn validation_refuses_a_dangling_required_cap_and_a_duplicate_id() {
    assert!(validate_catalog(&sample()).is_ok());
    let mut cat = sample();
    cat.method = vec![MethodRow {
        id: "mission.read".into(),
        required_cap: Some("mission.raed".into()),
        inline: false,
        open: false,
    }];
    let problems = validate_catalog(&cat).unwrap_err();
    assert!(problems[0].contains("unknown capability `mission.raed`"));
    let mut cat = sample();
    cat.agent.push(Cap {
        id: "event.publish".into(),
        label: "Dup".into(),
        description: "d".into(),
        category: "data_network".into(),
        risk: "low".into(),
        risk_reason: "r".into(),
        enforced: false,
    });
    assert!(validate_catalog(&cat).unwrap_err()[0].contains("duplicate agent id"));
}

#[test]
fn a_method_must_name_exactly_one_gate() {
    let mut cat = sample();
    cat.method = sample_methods();
    cat.method[1].required_cap = Some("mavlink.read".into());
    assert!(validate_catalog(&cat).is_ok());
    for (required_cap, inline, open) in [
        (None, false, false),
        (Some("event.publish".to_string()), true, false),
        (Some("event.publish".to_string()), false, true),
        (None, true, true),
    ] {
        let mut cat = sample();
        cat.method = vec![MethodRow {
            id: "new.method".into(),
            required_cap,
            inline,
            open,
        }];
        let problems = validate_catalog(&cat).unwrap_err();
        assert!(problems[0].contains("exactly one"), "{problems:?}");
    }
}

fn sample_methods() -> Vec<MethodRow> {
    vec![
        MethodRow {
            id: "ping".into(),
            required_cap: None,
            inline: false,
            open: true,
        },
        MethodRow {
            id: "mission.read".into(),
            required_cap: Some("mission.read".into()),
            inline: false,
            open: false,
        },
        MethodRow {
            id: "mavlink.register_component".into(),
            required_cap: None,
            inline: true,
            open: false,
        },
    ]
}

#[test]
fn rust_dispatch_emits_const_and_lookup() {
    let rs = emit_rust_dispatch(&sample_methods());
    assert!(rs.contains("pub const DISPATCH_METHODS: &[DispatchMethod]"));
    assert!(rs.contains("DispatchMethod { method: \"ping\", required_cap: None, inline: false }"));
    assert!(rs.contains(
        "DispatchMethod { method: \"mission.read\", required_cap: Some(\"mission.read\"), inline: false }"
    ));
    assert!(rs.contains(
        "DispatchMethod { method: \"mavlink.register_component\", required_cap: None, inline: true }"
    ));
    assert!(rs.contains("pub fn required_cap_for"));
    assert!(rs.contains("pub fn is_known_method"));
}

#[test]
fn python_dispatch_emits_required_cap_map() {
    let py = emit_python_dispatch(&sample_methods());
    assert!(py.contains("REQUIRED_CAP: dict[str, str | None]"));
    assert!(py.contains("\"ping\": None,"));
    assert!(py.contains("\"mission.read\": \"mission.read\","));
    assert!(py.contains("\"mavlink.register_component\": None,"));
    // The inline-gated method is present in the inline set, not None-equals-open.
    let inline_block = py
        .split("INLINE_GATED")
        .nth(1)
        .unwrap()
        .split("KNOWN_METHODS")
        .next()
        .unwrap();
    assert!(inline_block.contains("mavlink.register_component"));
    assert!(!inline_block.contains("\"ping\""));
}

fn sample_wfb() -> WfbTables {
    WfbTables {
        compatible: vec![WfbCompatible {
            vid: 0x0BDA,
            pid: 0xB812,
            label: "RTL8812EU".into(),
        }],
        driver: vec![WfbDriver {
            name: "rtl88x2eu".into(),
        }],
        deny_vendor: vec![WfbDenyVendor {
            vid: 0xA69C,
            note: "AIC8800".into(),
        }],
        deny_driver: vec![WfbDenyDriver {
            prefix: "brcmfmac".into(),
            note: "Broadcom".into(),
        }],
    }
}

#[test]
fn rust_wfb_emits_uppercase_hex_consts() {
    let rs = emit_rust_wfb(&sample_wfb());
    assert!(rs.contains("pub const WFB_COMPATIBLE: &[(u16, u16, &str)]"));
    assert!(rs.contains("(0x0BDA, 0xB812, \"RTL8812EU\"),"));
    assert!(rs.contains("pub const WFB_COMPATIBLE_DRIVERS: &[&str]"));
    assert!(rs.contains("\"rtl88x2eu\","));
    assert!(rs.contains("pub const DENY_VID: &[u16] = &[0xA69C];"));
    assert!(rs.contains("pub const DENY_DRIVER_PREFIXES: &[&str] = &[\"brcmfmac\"];"));
}

#[test]
fn python_wfb_emits_dict_and_deny_sets() {
    let py = emit_python_wfb(&sample_wfb());
    assert!(py.contains("WFB_COMPATIBLE: dict[tuple[int, int], str]"));
    assert!(py.contains("(0x0BDA, 0xB812): \"RTL8812EU\","));
    assert!(py.contains("WFB_COMPATIBLE_DRIVERS: frozenset[str]"));
    assert!(py.contains("WFB_DENY_USB_VENDORS: frozenset[int]"));
    assert!(py.contains("0xA69C,  # AIC8800"));
    assert!(py.contains("WFB_DENY_DRIVER_PREFIXES: tuple[str, ...]"));
    assert!(py.contains("\"brcmfmac\",  # Broadcom"));
}

fn sample_contracts() -> Contracts {
    Contracts {
        contract: vec![
            ContractRow {
                id: "plugin.rpc".into(),
                version: 1,
                wire: Some("msgpack".into()),
                transport: Some("unix-stream".into()),
                status: Some("active".into()),
                rust_const: Some("ados_protocol::plugin::PROTOCOL_VERSION".into()),
                description: Some("Plugin RPC envelope.".into()),
            },
            ContractRow {
                id: "state.v2".into(),
                version: 2,
                wire: Some("msgpack".into()),
                transport: Some("unix-stream".into()),
                status: Some("active".into()),
                rust_const: None,
                description: Some("Length-prefixed msgpack state frame.".into()),
            },
        ],
        sidecar: vec![SidecarRow {
            id: "wfb-stats".into(),
            version: 1,
            path: "/run/ados/wfb-stats.json".into(),
            max_age_ms: Some(10_000),
            writer: "ados-radio".into(),
            rust_const: Some("ados_protocol::paths::WFB_STATS_JSON".into()),
            description: Some("WFB link statistics.".into()),
        }],
        socket: Vec::new(),
        profiles: Profiles {
            values: vec!["drone".into(), "visor".into()],
        },
    }
}

#[test]
fn rust_contracts_emits_consts_and_lookups() {
    let rs = emit_rust_contracts(&sample_contracts());
    assert!(rs.contains("pub const CONTRACTS: &[ContractMeta]"));
    assert!(rs.contains("pub const SIDECARS: &[ContractMeta]"));
    assert!(rs.contains("id: \"plugin.rpc\", version: 1"));
    assert!(rs.contains("id: \"state.v2\", version: 2"));
    // The sidecar path is carried in `transport`, status is `sidecar`.
    assert!(rs.contains(
        "id: \"wfb-stats\", version: 1, wire: \"json\", transport: \"/run/ados/wfb-stats.json\", status: \"sidecar\""
    ));
    // rust_const is emitted as a trailing provenance comment, not a struct field.
    assert!(rs.contains(" },  // ados_protocol::plugin::PROTOCOL_VERSION"));
    assert!(rs.contains("pub const fn contract_version"));
    assert!(rs.contains("pub const fn sidecar_version"));
    assert!(rs.contains("pub const PROFILES: &[&str] = &[\"drone\", \"visor\"];"));
}

#[test]
fn python_contracts_emits_version_maps_and_catalog() {
    let py = emit_python_contracts(&sample_contracts());
    assert!(py.contains("CONTRACT_VERSIONS: dict[str, int]"));
    assert!(py.contains("\"plugin.rpc\": 1,"));
    assert!(py.contains("\"state.v2\": 2,"));
    assert!(py.contains("SIDECAR_VERSIONS: dict[str, int]"));
    assert!(py.contains("\"wfb-stats\": 1,"));
    assert!(py.contains("CONTRACT_CATALOG: dict[str, dict[str, object]]"));
    assert!(py.contains("SIDECAR_CATALOG: dict[str, dict[str, object]]"));
    assert!(py.contains("PROFILES: tuple[str, ...] = (\"drone\", \"visor\")"));
}

#[test]
fn ts_contracts_emits_const_version_maps_and_catalog() {
    let ts = emit_ts_contracts(&sample_contracts());
    assert!(ts.contains("export const CONTRACT_VERSIONS = {"));
    assert!(ts.contains("\"plugin.rpc\": 1,"));
    assert!(ts.contains("} as const;"));
    assert!(ts.contains("export const SIDECAR_VERSIONS = {"));
    assert!(ts.contains("export interface ContractMeta {"));
    assert!(ts.contains("export const CONTRACT_CATALOG: Record<string, ContractMeta>"));
    assert!(ts.contains("export const SIDECAR_CATALOG: Record<string, ContractMeta>"));
    assert!(ts.contains("export const PROFILES = [\"drone\", \"visor\"] as const;"));
}

/// Every ASCII integer on a line, in order (the header counts are the only
/// integers on their comment lines).
fn integers_in(line: &str) -> Vec<usize> {
    line.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<usize>().ok())
        .collect()
}

/// The first source line containing `needle`.
fn find_line<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line contains {needle:?}"))
}

#[test]
fn header_comment_counts_match_the_catalog() {
    // The `capabilities.toml` section headers carry parenthesised counts
    // (`AGENT capabilities (49)`, `GCS capabilities (20): 10 UI slots + 10 …`,
    // `dispatch table (43)`). They are hand-written documentation that silently
    // rots when a capability is added; pin them to the real entry counts so a
    // drift fails here, not at a confused reader.
    let toml_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("ados-protocol/capabilities.toml");
    let raw = std::fs::read_to_string(&toml_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", toml_path.display()));
    let cat: Catalog = toml::from_str(&raw).expect("parse capabilities.toml");

    let agent_count = cat.agent.len();
    let gcs_count = cat.gcs.len();
    let ui_slot_count = cat.gcs.iter().filter(|c| c.category == "ui_slot").count();
    let method_count = cat.method.len();

    // AGENT header: the single integer is the agent-capability count.
    let agent_header = integers_in(find_line(&raw, "AGENT capabilities ("));
    assert_eq!(
        agent_header.first().copied(),
        Some(agent_count),
        "AGENT capabilities header count != {agent_count} actual"
    );

    // GCS header: (total): N UI slots + M data — total/ui/data must all agree.
    let gcs_header = integers_in(find_line(&raw, "GCS capabilities ("));
    assert_eq!(
        gcs_header,
        vec![gcs_count, ui_slot_count, gcs_count - ui_slot_count],
        "GCS capabilities header (total: ui + data) != actual"
    );

    // Dispatch header: the single integer is the dispatch-method count.
    let dispatch_header = integers_in(find_line(&raw, "dispatch table ("));
    assert_eq!(
        dispatch_header.first().copied(),
        Some(method_count),
        "dispatch table header count != {method_count} actual"
    );
}

#[test]
fn gcs_mirror_manifest_pins_only_optional_targets_and_reflects_content() {
    let mono = PathBuf::from("/mono");
    let targets = vec![
        Target {
            path: mono.join("ADOSDroneAgent/crates/ados-protocol/src/capabilities.rs"),
            content: "rust agent side".into(),
            optional: false,
        },
        Target {
            path: mono.join("ADOSMissionControl/src/lib/plugins/contracts.generated.ts"),
            content: "ts one".into(),
            optional: true,
        },
        Target {
            path: mono.join("ADOSMissionControl/src/lib/plugins/gcs-capabilities.generated.ts"),
            content: "ts two".into(),
            optional: true,
        },
    ];
    let m = mirror_manifest(&targets, &mono, "GCS");

    // Only the optional (GCS-mirror) targets are pinned; the agent-side
    // non-optional target must not appear (it is checked directly).
    assert!(m.contains("ADOSMissionControl/src/lib/plugins/contracts.generated.ts"));
    assert!(m.contains("ADOSMissionControl/src/lib/plugins/gcs-capabilities.generated.ts"));
    assert!(
        !m.contains("capabilities.rs"),
        "non-optional target leaked into the manifest"
    );

    // The hash is the sha256 of the expected content, so a content change
    // changes the manifest — this is what makes agent-only CI bite.
    let one = hex::encode(Sha256::digest(b"ts one"));
    assert!(
        m.contains(&one),
        "manifest must carry the expected-content hash"
    );

    // Deterministic + sorted: regenerating identical input yields identical text.
    assert_eq!(m, mirror_manifest(&targets, &mono, "GCS"));

    // A changed mirror content produces a different manifest (the drift signal).
    let mut changed = targets;
    changed[1].content = "ts one CHANGED".into();
    assert_ne!(m, mirror_manifest(&changed, &mono, "GCS"));
}
