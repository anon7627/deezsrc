mod account;
mod crypto;
mod deezer;
mod plugin;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// deezsrc — Deezer source plugin.
#[derive(Parser, Debug)]
#[command(name = "deezsrc", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run as a source plugin: JSON-RPC on stdin/stdout, logs on stderr.
    /// Started by the host player, not by hand.
    Plugin {
        /// Origin of the web gateway, `www.deezer.com` (tests point it at a
        /// mock server).
        #[arg(long, hide = true)]
        gw_base: Option<String>,
        /// Root of the public API, `api.deezer.com`.
        #[arg(long, hide = true)]
        api_base: Option<String>,
        /// Root of the media service, `media.deezer.com`.
        #[arg(long, hide = true)]
        media_base: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let Command::Plugin { gw_base, api_base, media_base } = Cli::parse().command;
    // stdout carries the protocol; the host copies stderr to its log.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let endpoints = deezer::Endpoints::new(gw_base.as_deref(), api_base.as_deref(), media_base.as_deref());
    plugin::run(endpoints).await
}
