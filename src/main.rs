use std::io::IsTerminal;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::io::AsyncReadExt;
use tracing_subscriber::EnvFilter;

use clip_daemon::{
    backend::{ClipboardBackend, MAX_WAYLAND_SELECTION_BYTES},
    client, daemon,
    history::HistoryBackend,
    protocol,
};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run clipboard capture and D-Bus with embedded Turso history storage.
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
    /// Open and verify the history store without enabling clipboard capture.
    StoreStatus,
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
        Command::Daemon => {
            let backend = tokio::task::spawn_blocking(HistoryBackend::open).await??;
            daemon::run(backend).await
        }
        Command::Client => client::run().await,
        Command::Publish { mime } => publish_stdin(&mime).await,
        Command::Screenshot { screen, annotate } => client::screenshot(screen, annotate).await,
        Command::StoreStatus => store_status().await,
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

async fn store_status() -> Result<()> {
    let backend = tokio::task::spawn_blocking(HistoryBackend::open).await??;
    let status = backend.status().await;
    // Never print clipboard IDs, previews, MIME values, or content.
    let report = serde_json::to_string_pretty(&status);
    let shutdown = backend.shutdown().await;
    println!("{}", report?);
    shutdown?;
    anyhow::ensure!(status.available, "History store is unavailable");
    Ok(())
}
