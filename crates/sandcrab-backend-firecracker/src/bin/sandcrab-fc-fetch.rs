//! `sandcrab-fc-fetch`: download + SHA-256-verify the pinned guest kernel and Firecracker
//! release binaries. Wrapped by `scripts/fetch-artifacts.sh`. (Rootfs builds moved to the
//! `sandcrab-rootfs` binary in the `sandcrab-rootfs` crate.)

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use sandcrab_backend_firecracker::{artifacts, default_cache_root};

#[derive(Parser)]
#[command(
    name = "sandcrab-fc-fetch",
    about = "Fetch the pinned Firecracker kernel and binaries"
)]
struct Cli {
    /// Cache root (default: $SANDCRAB_HOME/cache or ~/.sandcrab/cache).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Skip the jailer binary.
    #[arg(long)]
    no_jailer: bool,
}

fn run(cli: Cli) -> Result<(), String> {
    let cache = match cli.out {
        Some(p) => p,
        None => default_cache_root().map_err(|e| e.to_string())?,
    };
    let k = artifacts::fetch_kernel(&cache).map_err(|e| e.to_string())?;
    let fc = artifacts::fetch_firecracker(&cache, !cli.no_jailer).map_err(|e| e.to_string())?;
    println!("kernel      {}", k.display());
    println!("firecracker {}", fc.firecracker.display());
    if let Some(j) = fc.jailer {
        println!("jailer      {}", j.display());
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
