use std::{io::IsTerminal, sync::Arc};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use clip_daemon::{
    backend::{ClipboardBackend, HistoryQuery},
    client, daemon, protocol,
    ringboard::RingboardBackend,
};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the session D-Bus service backed by Ringboard.
    Daemon,
    /// Bridge JSON Lines on stdin/stdout to the session service.
    Client,
    /// Check whether the pinned Ringboard database is readable.
    ProbeRingboard,
    /// Print stable protocol metadata and fixtures.
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
}

#[derive(Debug, Subcommand)]
enum DebugCommand {
    ProtocolRegistry,
    ContractFixture,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("clip_daemon=debug")),
        )
        .init();
    match Cli::parse().command {
        Command::Daemon => daemon::run(Arc::new(RingboardBackend::default())).await,
        Command::Client => client::run().await,
        Command::ProbeRingboard => {
            let backend = RingboardBackend::default();
            let status = backend.status().await;
            if !status.available {
                println!("{}", serde_json::to_string_pretty(&status)?);
                anyhow::bail!("Ringboard is unavailable");
            }
            let history = backend
                .query(HistoryQuery {
                    query: String::new(),
                    generation: 0,
                    limit: 10,
                })
                .await?;
            // Never print clipboard IDs, previews, MIME values, or content from a probe.
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": status,
                    "query": {
                        "entries_returned": history.entries.len(),
                        "has_more": history.has_more,
                        "revision": history.revision
                    }
                }))?
            );
            Ok(())
        }
        Command::Debug { command } => {
            let value = match command {
                DebugCommand::ProtocolRegistry => protocol::registry(),
                DebugCommand::ContractFixture => protocol::contract_fixture(),
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
    }
}
