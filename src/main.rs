mod audiokey;
mod connect;
mod plugin;
mod session;
mod webapi;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// spconnect — unofficial Spotify source plugin.
#[derive(Parser, Debug)]
#[command(name = "spconnect", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run as a source plugin: JSON-RPC on stdin/stdout, logs on stderr.
    /// Started by the host player, not by hand.
    Plugin {
        /// Web API root (tests point it at a mock server).
        #[arg(long, hide = true)]
        api_base: Option<String>,
        /// Accounts service root, for OAuth (tests point it at a mock server).
        #[arg(long, hide = true)]
        accounts_base: Option<String>,
        /// Never connect to Spotify's access points: Web API only, with the
        /// OAuth token (tests).
        #[arg(long, hide = true)]
        no_session: bool,
    },
}

/// librespot logs the account name at sign-in, and tokens at trace level:
/// these targets stay quiet whatever `RUST_LOG` says.
const QUIET: &[&str] = &[
    "librespot_core::session=warn",
    "librespot_core::connection=warn",
    "librespot_core::login5=info",
    "librespot_core::token=info",
    "librespot_core::dealer=info",
    "librespot_core::spclient=info",
    "librespot_core::http_client=info",
    "librespot_core::cdn_url=info",
];

fn log_filter() -> EnvFilter {
    let base = std::env::var("RUST_LOG").unwrap_or_else(|_| "info,librespot=warn,librespot_core=warn".into());
    let spec = std::iter::once(base.as_str()).chain(QUIET.iter().copied()).collect::<Vec<_>>().join(",");
    EnvFilter::try_new(&spec).unwrap_or_else(|_| EnvFilter::new("info"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let Command::Plugin { api_base, accounts_base, no_session } = Cli::parse().command;
    // stdout carries the protocol; the host copies stderr to its log.
    tracing_subscriber::fmt().with_env_filter(log_filter()).with_writer(std::io::stderr).with_ansi(false).init();
    plugin::run(plugin::Options { api_base, accounts_base, no_session }).await
}
