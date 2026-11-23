//! `crabd` — lightweight Rust guest daemon running inside every Sandcrab sandbox.
//!
//! Phase 1B implementation plan (see `docs/ROADMAP.md`):
//! - Transport listeners: `--vsock <port>` (Linux microVMs), `--unix <path>`, `--tcp <addr>`
//! - Native Rust tools: `bash` (killpg + output truncation), `file_read`, `file_write`,
//!   `file_edit`, `file_delete`, `file_move`, `list_dir`, `glob`, `grep`, `process_*`

use std::path::PathBuf;

use clap::Parser;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "crabd", version, about = "Sandcrab guest daemon")]
struct Args {
    /// Listen on a Unix domain socket path (used by DockerDev and local testing).
    #[arg(long)]
    unix: Option<PathBuf>,

    /// Listen on a TCP address (e.g. 0.0.0.0:5000).
    #[arg(long)]
    tcp: Option<String>,

    /// Listen on a Linux virtio-vsock port (used by Firecracker, Cloud Hypervisor, libkrun).
    #[arg(long)]
    vsock: Option<u32>,

    /// Default working directory for relative tool paths and bash invocations.
    #[arg(long, default_value = "/workspace")]
    default_cwd: PathBuf,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    info!(
        ?args.unix,
        ?args.tcp,
        ?args.vsock,
        default_cwd = %args.default_cwd.display(),
        tools = sandcrab_protocol::tool_definitions().len(),
        "crabd scaffold initialized (Phase 1B RPC listener and tool handlers pending)"
    );
}
