//! `ados-ota` oneshot update poller.
//!
//! Polls the GitHub Releases API once for a newer full-agent release (ETag
//! cache, full-agent tag filter, SHA256SUMS lookup) and reports the result. The
//! daily loop / install is the agent's job; this is the standalone oneshot the
//! agent's update path invokes. Reads the channel + repo from the cloud config.

use anyhow::Result;

use ados_cloud::config::CloudConfig;
use ados_cloud::ota::{GithubSource, UpdateChecker, UpdateConfig};

fn init_logging() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    #[cfg(target_os = "linux")]
    {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::EnvFilter;
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&filter))
        .try_init();
}

fn main() -> Result<()> {
    init_logging();

    let config = CloudConfig::load();
    let current = env!("CARGO_PKG_VERSION");
    let update_config = UpdateConfig {
        channel: config.ota.channel.clone(),
        github_repo: config.ota.github_repo.clone(),
    };
    tracing::info!(
        channel = %update_config.channel,
        repo = %update_config.github_repo,
        current = %current,
        "checking for update"
    );

    let mut checker = UpdateChecker::new(update_config, GithubSource::new());
    match checker.check_for_update(current) {
        Some(manifest) => {
            tracing::info!(
                version = %manifest.version,
                url = %manifest.download_url,
                "update available"
            );
            // Print a small machine-readable line for the caller (the install
            // path decides whether to download + apply).
            println!(
                "{}",
                serde_json::json!({
                    "updateAvailable": true,
                    "version": manifest.version,
                    "downloadUrl": manifest.download_url,
                    "sha256": manifest.sha256,
                })
            );
        }
        None => {
            tracing::info!("no update available");
            println!("{}", serde_json::json!({"updateAvailable": false}));
        }
    }
    Ok(())
}
