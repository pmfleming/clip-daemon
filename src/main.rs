use std::{io::IsTerminal, sync::Arc};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::io::AsyncReadExt;
use tracing_subscriber::EnvFilter;

use clip_daemon::{
    backend::{ClipboardBackend, HistoryQuery, MAX_WAYLAND_SELECTION_BYTES},
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
    /// Publish stdin to the clipboard through the running daemon.
    Publish {
        /// MIME type of the stdin content.
        #[arg(long)]
        mime: String,
    },
    /// Start a daemon-owned screenshot (region selection by default).
    Screenshot {
        #[arg(long)]
        screen: bool,
        #[arg(long)]
        annotate: bool,
    },
    /// Persist validated native settings before starting the Ringboard server.
    ConfigureEngine,
    /// systemd ExecCondition: fail closed unless persisted intent allows capture.
    CaptureAllowed,
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
    run(Cli::parse().command).await
}

async fn run(command: Command) -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("clip_daemon=debug")),
        )
        .init();
    match command {
        Command::Daemon => daemon::run(Arc::new(RingboardBackend::default())).await,
        Command::Client => client::run().await,
        Command::Publish { mime } => publish_stdin(&mime).await,
        Command::Screenshot { screen, annotate } => client::screenshot(screen, annotate).await,
        Command::ConfigureEngine => clip_daemon::settings::SettingsManager::default()
            .prepare_engine()
            .map_err(anyhow::Error::msg),
        Command::CaptureAllowed => {
            let settings = clip_daemon::settings::SettingsManager::default();
            anyhow::ensure!(
                settings.capture_allowed().map_err(anyhow::Error::msg)?,
                "Capture is disabled by persisted preference"
            );
            Ok(())
        }
        Command::ProbeRingboard => probe_ringboard().await,
        Command::Debug { command } => print_debug(command),
    }
}

fn print_debug(command: DebugCommand) -> Result<()> {
    let value = match command {
        DebugCommand::ProtocolRegistry => protocol::registry()?,
        DebugCommand::ContractFixture => protocol::contract_fixture()?,
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn publish_stdin(mime: &str) -> Result<()> {
    let mut bytes = Vec::new();
    tokio::io::stdin()
        .take(MAX_WAYLAND_SELECTION_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_WAYLAND_SELECTION_BYTES,
        "stdin exceeds the hard clipboard limit of {MAX_WAYLAND_SELECTION_BYTES} bytes"
    );
    client::publish(mime, bytes).await
}

async fn probe_ringboard() -> Result<()> {
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
            offset: 0,
            limit: 10,
            collapse_self_echoes: true,
        })
        .await?;
    // Never print clipboard IDs, previews, MIME values, or content from a probe.
    let report = serde_json::json!({
        "status": status,
        "query": {
            "entries_returned": history.entries.len(),
            "has_more": history.has_more,
            "revision": history.revision
        }
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
