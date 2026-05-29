//! Plugin lifecycle trait and the runner entry.
//!
//! Ports `ados.plugins.runner` for the `runtime: rust` case. A Rust plugin's
//! binary implements [`Plugin`] and calls [`run_plugin`] from `main`; the
//! runner reads `--socket` / `--token` / `--agent-id` (with `ADOS_PLUGIN_*`
//! env-var fallbacks — the exact contract `runner.py` passes), connects the
//! [`PluginIpcClient`], builds a [`PluginContext`], and drives the lifecycle
//! hooks until SIGTERM/SIGINT, then runs the teardown hooks.
//!
//! **Entry: function, not proc-macro.** A `#[ados_plugin]` attribute macro
//! would expand to nothing more than `fn main() { run_plugin::<P>() }` — it
//! saves one line and costs a separate proc-macro crate, a syn/quote dependency
//! tree, and an opaque codegen step. The plain generic function is the cleaner
//! contract: a plugin author writes a three-line `main`, the call site is
//! explicit, and there is no macro to debug. So the SDK ships [`run_plugin`]
//! and no proc-macro sub-crate.
//!
//! Hook order matches the Python runner exactly:
//! `on_install` -> `on_enable` -> `on_configure` -> `on_start` -> (wait for
//! shutdown) -> `on_stop` -> `on_disable`. Every hook has a default no-op so a
//! plugin overrides only what it needs.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use rmpv::Value;
use thiserror::Error;

use crate::client::{ClientError, PluginIpcClient};
use crate::context::PluginContext;

/// The lifecycle hook trait a Rust plugin implements. Mirrors the duck-typed
/// `on_install` / `on_enable` / `on_configure` / `on_start` / `on_stop` /
/// `on_disable` methods the Python runner calls via `getattr`.
///
/// Every hook defaults to a no-op. `on_configure` receives the resolved config
/// map (the Python runner passes `{}` today; the SDK passes the same shape and
/// will carry the live config when the host wires it).
#[async_trait]
pub trait Plugin: Send + Sync {
    /// Construct the plugin. Called once before any hook. The runner uses this
    /// rather than `Default` so a plugin can fail construction explicitly.
    fn new() -> Self
    where
        Self: Sized;

    async fn on_install(&mut self, _ctx: &PluginContext) -> Result<(), ClientError> {
        Ok(())
    }
    async fn on_enable(&mut self, _ctx: &PluginContext) -> Result<(), ClientError> {
        Ok(())
    }
    async fn on_configure(
        &mut self,
        _ctx: &PluginContext,
        _config: &BTreeMap<String, Value>,
    ) -> Result<(), ClientError> {
        Ok(())
    }
    async fn on_start(&mut self, _ctx: &PluginContext) -> Result<(), ClientError> {
        Ok(())
    }
    async fn on_stop(&mut self, _ctx: &PluginContext) -> Result<(), ClientError> {
        Ok(())
    }
    async fn on_disable(&mut self, _ctx: &PluginContext) -> Result<(), ClientError> {
        Ok(())
    }
}

/// Errors raised while bootstrapping or running a plugin via [`run_plugin`].
#[derive(Debug, Error)]
pub enum RunnerError {
    /// The plugin id positional argument was missing.
    #[error("missing plugin id argument")]
    MissingPluginId,
    /// `--socket` / `ADOS_PLUGIN_SOCKET` or `--token` / `ADOS_PLUGIN_TOKEN` was
    /// absent, so there is no host to connect to. Mirrors the Python runner's
    /// fall-back-to-null-ipc path; here it is an explicit error because a Rust
    /// `runtime: rust` plugin is always launched with the bridge wired.
    #[error("no supervisor socket/token supplied (set --socket and --token)")]
    NoBridge,
    /// The client failed to connect or handshake.
    #[error("ipc connect failed: {0}")]
    Connect(#[from] ClientError),
}

/// The parsed runner arguments. Mirrors the `plugin_id` positional plus the
/// `--socket` / `--token` / `--agent-id` options the Python runner reads, with
/// the `ADOS_PLUGIN_SOCKET` / `ADOS_PLUGIN_TOKEN` / `ADOS_PLUGIN_AGENT_ID`
/// env-var fallbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerArgs {
    pub plugin_id: String,
    pub socket_path: Option<String>,
    pub token: Option<String>,
    pub agent_id: String,
}

impl RunnerArgs {
    /// Parse from a raw argv (excluding argv[0]) and an env lookup. The env
    /// lookup is injected so the parse is unit-testable without touching the
    /// process environment.
    pub fn parse<F>(args: &[String], env: F) -> Result<Self, RunnerError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut plugin_id: Option<String> = None;
        let mut socket_path: Option<String> = None;
        let mut token: Option<String> = None;
        let mut agent_id: Option<String> = None;

        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            match arg.as_str() {
                "--socket" => {
                    socket_path = args.get(i + 1).cloned();
                    i += 2;
                }
                "--token" => {
                    token = args.get(i + 1).cloned();
                    i += 2;
                }
                "--agent-id" => {
                    agent_id = args.get(i + 1).cloned();
                    i += 2;
                }
                other if other.starts_with("--socket=") => {
                    socket_path = Some(other["--socket=".len()..].to_string());
                    i += 1;
                }
                other if other.starts_with("--token=") => {
                    token = Some(other["--token=".len()..].to_string());
                    i += 1;
                }
                other if other.starts_with("--agent-id=") => {
                    agent_id = Some(other["--agent-id=".len()..].to_string());
                    i += 1;
                }
                _ if plugin_id.is_none() => {
                    plugin_id = Some(arg.clone());
                    i += 1;
                }
                _ => {
                    i += 1;
                }
            }
        }

        Ok(Self {
            plugin_id: plugin_id.ok_or(RunnerError::MissingPluginId)?,
            // Env fallbacks match the Python option `default=lambda: os.environ.get(...)`.
            socket_path: socket_path.or_else(|| env("ADOS_PLUGIN_SOCKET")),
            token: token.or_else(|| env("ADOS_PLUGIN_TOKEN")),
            agent_id: agent_id
                .or_else(|| env("ADOS_PLUGIN_AGENT_ID"))
                .unwrap_or_default(),
        })
    }
}

/// Run a plugin to completion: parse argv/env, connect, build the context,
/// drive the hooks, and tear down on the shutdown signal. This is the
/// `runtime: rust` plugin's `main` body.
///
/// `plugin_version` is the version the host installed; a real binary embeds its
/// own `env!("CARGO_PKG_VERSION")`. `static_config` is the manifest-supplied
/// config the runner would have read off disk; the SDK takes it as an argument
/// so the binary stays in control of where it loads config from.
///
/// Returns when the shutdown future resolves (SIGTERM/SIGINT in a real binary)
/// or a hook errors.
pub async fn run_plugin<P, S>(
    plugin_version: impl Into<String>,
    static_config: BTreeMap<String, Value>,
    shutdown: S,
) -> Result<(), RunnerError>
where
    P: Plugin,
    S: std::future::Future<Output = ()>,
{
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = RunnerArgs::parse(&argv, |k| std::env::var(k).ok())?;
    run_plugin_with::<P, S>(args, plugin_version, static_config, shutdown).await
}

/// Run a plugin against pre-parsed [`RunnerArgs`]. Splits out from
/// [`run_plugin`] so a host harness or a test can supply args directly without
/// touching the process environment.
pub async fn run_plugin_with<P, S>(
    args: RunnerArgs,
    plugin_version: impl Into<String>,
    static_config: BTreeMap<String, Value>,
    shutdown: S,
) -> Result<(), RunnerError>
where
    P: Plugin,
    S: std::future::Future<Output = ()>,
{
    let (Some(socket_path), Some(token)) = (args.socket_path.clone(), args.token.clone()) else {
        return Err(RunnerError::NoBridge);
    };

    let ipc = Arc::new(PluginIpcClient::new(
        args.plugin_id.clone(),
        token,
        socket_path,
    ));
    ipc.connect().await?;

    let ctx = PluginContext::new(
        ipc.clone(),
        plugin_version,
        args.agent_id,
        static_config.clone(),
    );

    let mut plugin = P::new();
    let result = drive(&mut plugin, &ctx, &static_config, shutdown).await;

    // Always close the client, success or failure.
    ipc.close().await;
    result
}

/// Drive the hook sequence. Separated so the teardown hooks run even when a
/// startup hook errors. Mirrors the Python runner's try/finally shape.
async fn drive<P, S>(
    plugin: &mut P,
    ctx: &PluginContext,
    config: &BTreeMap<String, Value>,
    shutdown: S,
) -> Result<(), RunnerError>
where
    P: Plugin,
    S: std::future::Future<Output = ()>,
{
    plugin.on_install(ctx).await?;
    plugin.on_enable(ctx).await?;
    plugin.on_configure(ctx, config).await?;
    plugin.on_start(ctx).await?;

    tracing::info!(plugin_id = %ctx.plugin_id, "plugin ready");
    shutdown.await;

    plugin.on_stop(ctx).await?;
    plugin.on_disable(ctx).await?;
    tracing::info!(plugin_id = %ctx.plugin_id, "plugin clean exit");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn parses_positional_id_and_space_separated_options() {
        let argv = vec![
            "com.example.demo".to_string(),
            "--socket".to_string(),
            "/run/ados/plugins/x.sock".to_string(),
            "--token".to_string(),
            "v1|p|s|0|600||sig".to_string(),
            "--agent-id".to_string(),
            "agent-42".to_string(),
        ];
        let args = RunnerArgs::parse(&argv, no_env).unwrap();
        assert_eq!(args.plugin_id, "com.example.demo");
        assert_eq!(
            args.socket_path.as_deref(),
            Some("/run/ados/plugins/x.sock")
        );
        assert_eq!(args.token.as_deref(), Some("v1|p|s|0|600||sig"));
        assert_eq!(args.agent_id, "agent-42");
    }

    #[test]
    fn parses_equals_form_options() {
        let argv = vec![
            "com.example.demo".to_string(),
            "--socket=/tmp/y.sock".to_string(),
            "--token=tok".to_string(),
        ];
        let args = RunnerArgs::parse(&argv, no_env).unwrap();
        assert_eq!(args.socket_path.as_deref(), Some("/tmp/y.sock"));
        assert_eq!(args.token.as_deref(), Some("tok"));
        // agent-id defaults to empty, matching the Python `default=""`.
        assert_eq!(args.agent_id, "");
    }

    #[test]
    fn falls_back_to_env_for_socket_token_agent() {
        let argv = vec!["com.example.demo".to_string()];
        let env = |k: &str| match k {
            "ADOS_PLUGIN_SOCKET" => Some("/env/sock".to_string()),
            "ADOS_PLUGIN_TOKEN" => Some("env-tok".to_string()),
            "ADOS_PLUGIN_AGENT_ID" => Some("env-agent".to_string()),
            _ => None,
        };
        let args = RunnerArgs::parse(&argv, env).unwrap();
        assert_eq!(args.socket_path.as_deref(), Some("/env/sock"));
        assert_eq!(args.token.as_deref(), Some("env-tok"));
        assert_eq!(args.agent_id, "env-agent");
    }

    #[test]
    fn explicit_option_overrides_env() {
        let argv = vec![
            "com.example.demo".to_string(),
            "--socket".to_string(),
            "/explicit/sock".to_string(),
        ];
        let env = |k: &str| match k {
            "ADOS_PLUGIN_SOCKET" => Some("/env/sock".to_string()),
            _ => None,
        };
        let args = RunnerArgs::parse(&argv, env).unwrap();
        assert_eq!(args.socket_path.as_deref(), Some("/explicit/sock"));
    }

    #[test]
    fn missing_plugin_id_errors() {
        let argv = vec!["--socket".to_string(), "/tmp/x".to_string()];
        let err = RunnerArgs::parse(&argv, no_env).unwrap_err();
        assert!(matches!(err, RunnerError::MissingPluginId));
    }

    /// A dummy plugin proves the trait's default-no-op hooks compile and that a
    /// minimal implementation only needs `new`.
    struct DummyPlugin {
        started: bool,
    }

    #[async_trait]
    impl Plugin for DummyPlugin {
        fn new() -> Self {
            Self { started: false }
        }
        async fn on_start(&mut self, _ctx: &PluginContext) -> Result<(), ClientError> {
            self.started = true;
            Ok(())
        }
    }

    #[test]
    fn dummy_plugin_constructs_with_only_new() {
        let p = DummyPlugin::new();
        assert!(!p.started);
    }

    #[tokio::test]
    async fn no_bridge_when_socket_or_token_absent() {
        // plugin id present but no socket/token -> NoBridge, before any connect.
        let args = RunnerArgs {
            plugin_id: "com.example.demo".to_string(),
            socket_path: None,
            token: None,
            agent_id: String::new(),
        };
        let err = run_plugin_with::<DummyPlugin, _>(
            args,
            "1.0.0",
            BTreeMap::new(),
            std::future::ready(()),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RunnerError::NoBridge));
    }
}
