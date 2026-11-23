//! `crabagent` binary. See the crate README.

use std::process::ExitCode;

use clap::Parser;
use sandcrab_agent::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = match cli.verbose {
        0 => "warn",
        1 => "info,sandcrab=debug,sandcrab_agent=debug",
        _ => "debug",
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .init();

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(sandcrab_agent::session::main(cli)) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
