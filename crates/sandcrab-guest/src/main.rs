//! `crabd` — Sandcrab guest daemon.
//!
//! ```text
//! crabd serve (--unix <path> | --tcp <addr> | --vsock <port>) [--default-cwd /workspace]
//! crabd init        # PID 1 of a microVM (also selected automatically when PID == 1)
//! ```
//!
//! Subcommands:
//! - `serve`: answer framed JSON-RPC tool calls (see the `sandcrab_guest` library docs).
//! - `init`: PID 1 mode for microVMs that mounts the rootfs / workspaces, takes config over
//!   the vsock control port and spawns `crabd serve` (see `sandcrab_guest::init`).
//!
//! Logs go to stderr (`RUST_LOG`, default `info`).

use std::{path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

use clap::{Args, Parser, Subcommand};
use sandcrab_guest::{ListenAddr, Server, ServerConfig, ToolContext, server::shutdown_signal};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "crabd", version, about = "Sandcrab guest daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve tool calls over a Unix socket, TCP or vsock.
    Serve(ServeArgs),
    /// Run as PID 1 of a Sandcrab microVM (mounts, control port, spawns `serve`).
    Init,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[command(flatten)]
    listen: ListenArgs,

    /// Default working directory for relative tool paths and commands.
    #[arg(long, value_name = "PATH", default_value = "/workspace")]
    default_cwd: PathBuf,

    /// Max concurrently served connections.
    #[arg(long, value_name = "N", default_value_t = 64, value_parser = clap::value_parser!(u32).range(1..))]
    max_connections: u32,

    /// Max concurrently running requests per connection.
    #[arg(long, value_name = "N", default_value_t = 64, value_parser = clap::value_parser!(u32).range(1..))]
    max_inflight: u32,

    /// Grace period for in-flight requests on SIGTERM / SIGINT, in milliseconds.
    #[arg(long, value_name = "MS", default_value_t = 10_000)]
    shutdown_grace_ms: u64,
}

/// Exactly one transport.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct ListenArgs {
    /// Listen on a Unix domain socket (Docker, local testing). A stale socket is replaced.
    #[arg(long, value_name = "PATH")]
    unix: Option<PathBuf>,

    /// Listen on a TCP address, e.g. 127.0.0.1:5000. Unauthenticated: loopback / private nets only.
    #[arg(long, value_name = "ADDR")]
    tcp: Option<String>,

    /// Listen on a virtio-vsock port, any CID (Firecracker; Linux only).
    #[arg(long, value_name = "PORT")]
    vsock: Option<u32>,
}

impl ListenArgs {
    fn addr(self) -> ListenAddr {
        match (self.unix, self.tcp, self.vsock) {
            (Some(p), _, _) => ListenAddr::Unix(p),
            (_, Some(a), _) => ListenAddr::Tcp(a),
            (_, _, Some(port)) => ListenAddr::Vsock(port),
            (None, None, None) => unreachable!("clap requires exactly one transport"),
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // Before clap: the kernel starts `init=/sbin/crabd` without a subcommand.
    let args: Vec<_> = std::env::args_os().collect();
    if sandcrab_guest::init::is_init_invocation(std::process::id(), &args) {
        return sandcrab_guest::init::run().await;
    }

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve(args) => serve(args).await,
        Command::Init => unreachable!("handled before argument parsing"),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("crabd: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(args: ServeArgs) -> std::io::Result<()> {
    let ctx = Arc::new(ToolContext::new(args.default_cwd));
    if !ctx.default_cwd.is_dir() {
        warn!(default_cwd = %ctx.default_cwd.display(), "default cwd is not an existing directory");
    }
    let mut config = ServerConfig::new(args.listen.addr());
    config.max_connections = args.max_connections as usize;
    config.max_inflight_per_connection = args.max_inflight as usize;
    config.shutdown_grace = Duration::from_millis(args.shutdown_grace_ms);

    let shutdown = shutdown_signal()?;
    let server = Server::bind(config, ctx).await?;
    info!(
        version = sandcrab_guest::CRABD_VERSION,
        protocol = sandcrab_protocol::PROTOCOL_VERSION,
        endpoint = %server.endpoint(),
        "crabd listening"
    );
    server.run(shutdown).await
}
