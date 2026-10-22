//! The `claude mcp add` commands the MCP settings page shows and copies: one
//! that runs the published container image, one that runs a local clone.
//! Both strings match Mission Control's MCP tab byte for byte.

use serde_json::Value;

use super::kit;
use crate::app::Core;
use crate::prefs::host_for_url;
use crate::settings::row::MASK;

/// Stands in for the operator's local ADOS-MCP clone.
pub const CLONE_PATH: &str = "<path-to>/ADOS-MCP";
/// Stands in for the key when this session has none.
pub const KEY_PLACEHOLDER: &str = "<pairing-key>";
/// The published ADOS-MCP container image the Docker recipe runs.
pub const DOCKER_IMAGE: &str = "ghcr.io/altnautica/ados-mcp:latest";

/// How the operator runs the local MCP server.
#[derive(Clone, Copy)]
pub enum Runtime {
    Docker,
    Source,
}

/// The LAN-direct `claude mcp add` command for this agent.
pub fn connect_recipe(rt: Runtime, host: &str, api_key: &str) -> String {
    match rt {
        Runtime::Docker => format!(
            "claude mcp add ados -e ADOS_MCP_AGENT_KEY={api_key} -- docker run -i --rm -e ADOS_MCP_AGENT_KEY -v \"$HOME/.ados/mcp:/home/node/.ados/mcp\" {DOCKER_IMAGE} --target agent {host}"
        ),
        Runtime::Source => format!(
            "claude mcp add ados -e ADOS_MCP_AGENT_KEY={api_key} -- node {CLONE_PATH}/dist/index.js --target agent {host}"
        ),
    }
}

/// The recipe as displayed: the key masked, or the placeholder when unknown.
pub fn masked_recipe(rt: Runtime, host: &str, api_key: Option<&str>) -> String {
    connect_recipe(
        rt,
        host,
        if api_key.is_some() {
            MASK
        } else {
            KEY_PLACEHOLDER
        },
    )
}

/// The agent's API URL: the first present of the two setup hosts, else the
/// host this session targets, on the API port.
fn api_url(core: &Core, preferred: &str, fallback: &str) -> String {
    let s = kit::setup(core);
    let host = kit::text(s, preferred)
        .or_else(|| kit::text(s, fallback))
        .map(str::to_string)
        .unwrap_or_else(|| core.target.host.clone());
    let port = s
        .and_then(|v| crate::app::at(v, "network.api_port"))
        .and_then(Value::as_u64)
        .or_else(|| {
            core.target
                .base
                .rsplit(':')
                .next()
                .and_then(|p| p.parse().ok())
        })
        .unwrap_or(8080);
    format!("http://{}:{port}", host_for_url(&host))
}

/// The recipe's host for a runtime. From source prefers the setup mDNS name;
/// Docker prefers the LAN address, since `.local` names may not resolve
/// inside a container.
pub fn recipe_host(core: &Core, rt: Runtime) -> String {
    match rt {
        Runtime::Docker => api_url(core, "lan_host", "network.mdns_host"),
        Runtime::Source => api_url(core, "network.mdns_host", "lan_host"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poll::ResourceId;
    use serde_json::json;

    #[test]
    fn the_connect_recipes_match_mission_control() {
        let host = "http://drone.local:8080";
        assert_eq!(
            connect_recipe(Runtime::Source, host, "k3y"),
            "claude mcp add ados -e ADOS_MCP_AGENT_KEY=k3y -- node <path-to>/ADOS-MCP/dist/index.js --target agent http://drone.local:8080"
        );
        assert_eq!(
            connect_recipe(Runtime::Docker, host, "k3y"),
            "claude mcp add ados -e ADOS_MCP_AGENT_KEY=k3y -- docker run -i --rm -e ADOS_MCP_AGENT_KEY -v \"$HOME/.ados/mcp:/home/node/.ados/mcp\" ghcr.io/altnautica/ados-mcp:latest --target agent http://drone.local:8080"
        );
        for rt in [Runtime::Docker, Runtime::Source] {
            let shown = masked_recipe(rt, host, Some("k3y"));
            assert!(!shown.contains("k3y"), "{shown}");
            assert!(
                shown.contains(&format!("ADOS_MCP_AGENT_KEY={MASK} ")),
                "{shown}"
            );
            assert!(masked_recipe(rt, host, None).contains("ADOS_MCP_AGENT_KEY=<pairing-key> "));
        }
    }

    fn app_with_both_hosts() -> crate::app::App {
        let mut app = crate::ui::tests::test_app();
        app.core.res.insert(
            ResourceId::SetupStatus,
            crate::app::Slot {
                value: Some(json!({"network": {"mdns_host": "bench.local", "api_port": 8080}, "lan_host": "192.168.1.50"})),
                error: None,
                code: None,
                error_body: None,
                at: std::time::Instant::now(),
            },
        );
        app
    }

    #[test]
    fn the_source_host_prefers_the_mdns_name() {
        let app = app_with_both_hosts();
        assert_eq!(
            recipe_host(&app.core, Runtime::Source),
            "http://bench.local:8080"
        );
    }

    #[test]
    fn the_docker_host_prefers_the_lan_address() {
        let app = app_with_both_hosts();
        assert_eq!(
            recipe_host(&app.core, Runtime::Docker),
            "http://192.168.1.50:8080"
        );
    }
}
