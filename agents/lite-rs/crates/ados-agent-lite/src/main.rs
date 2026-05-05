// ados-agent-lite — main binary entry point.
//
// Single-process, single static binary. Cooperating tokio tasks handle the
// MAVLink router, cloud relay client, and a tiny axum HTTP server stub.
// Reads /etc/ados/agent.yaml for configuration and dispatches behavior per
// the detected board profile loaded from /opt/ados/hal/boards/<id>.yaml.

#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "ados-agent-lite")]
#[command(about = "Lightweight ADOS Drone Agent for low-RAM SBCs")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the agent (default when invoked without a subcommand).
    Run,

    /// Print agent status and exit.
    Status,

    /// Print version information and exit.
    Version,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run().await,
        Command::Status => {
            print_status();
            Ok(())
        }
        Command::Version => {
            println!("ados-agent-lite {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).compact().init();
}

fn print_status() {
    println!("ados-agent-lite {}", env!("CARGO_PKG_VERSION"));
    println!("status: placeholder — wire this to the live agent in the next phase");
}

async fn run() -> anyhow::Result<()> {
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "ados-agent-lite starting");

    // Cooperating tasks. Each placeholder returns immediately at v0.1; the
    // real implementations land in the next phase.
    let mavlink_handle = tokio::spawn(async {
        ados_mavlink::run_router().await
    });
    let cloud_handle = tokio::spawn(async {
        ados_cloud::run_cloud_client().await
    });

    // Wait for shutdown or any task to fail.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutdown signal received");
        }
        result = mavlink_handle => {
            handle_task_result("mavlink", result)?;
        }
        result = cloud_handle => {
            handle_task_result("cloud", result)?;
        }
    }

    tracing::info!("ados-agent-lite stopped");
    Ok(())
}

fn handle_task_result<E: std::fmt::Display>(
    name: &str,
    result: Result<Result<(), E>, tokio::task::JoinError>,
) -> anyhow::Result<()> {
    match result {
        Ok(Ok(())) => {
            tracing::info!(task = name, "task exited cleanly");
            Ok(())
        }
        Ok(Err(e)) => {
            tracing::error!(task = name, error = %e, "task failed");
            anyhow::bail!("{} task failed: {}", name, e);
        }
        Err(e) => {
            tracing::error!(task = name, error = %e, "task panicked");
            anyhow::bail!("{} task panicked: {}", name, e);
        }
    }
}
