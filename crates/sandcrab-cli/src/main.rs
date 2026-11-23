//! `sandcrab` — operator & developer CLI for Sandcrab.
//!
//! Exit codes: `run` exits with the sandboxed command's code (`128 + signal` if killed,
//! `130` on Ctrl-C); other commands exit `0` on success, `1` on error (`doctor`: any failed
//! check), `2` on usage errors.

mod args;
mod doctor;
mod run;
mod util;
mod workspace;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::args::{BackendArg, SpawnArgs, parse_backend};

#[derive(Debug, Parser)]
#[command(
    name = "sandcrab",
    version,
    about = "Ephemeral sandboxes with persistent workspaces for AI agents",
    after_help = "DockerDev (`--backend docker`) shares the host kernel: it is for local dev / CI \
                  only and requires --allow-insecure or SANDCRAB_ALLOW_INSECURE=1."
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Clone, clap::Args)]
pub struct Global {
    /// State root (workspace registry, sandbox sockets). Default: $SANDCRAB_HOME, else
    /// ~/.sandcrab.
    #[arg(long, global = true, env = "SANDCRAB_HOME")]
    pub state_dir: Option<PathBuf>,

    /// Template directory; repeatable, first wins. Default: ./templates, then
    /// <state root>/templates.
    #[arg(long = "templates-dir", global = true)]
    pub templates_dirs: Vec<PathBuf>,

    /// Runtime backend (explicit; no fallback). Available: docker.
    #[arg(long, global = true, env = "SANDCRAB_BACKEND", value_parser = parse_backend)]
    pub backend: Option<BackendArg>,

    /// Acknowledge that the docker backend shares the host kernel (dev / CI only). Also
    /// enabled by SANDCRAB_ALLOW_INSECURE=1.
    #[arg(long, global = true)]
    pub allow_insecure: bool,

    /// Host path of the static musl `crabd` (default: $SANDCRAB_CRABD, else
    /// target/<arch>-unknown-linux-musl/release/crabd).
    #[arg(long, global = true)]
    pub crabd: Option<PathBuf>,

    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    pub json: bool,

    /// Print lifecycle timings to stderr (-vv: debug logs).
    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Check the host: crabd, Docker, template images + pins, /dev/kvm, state dir.
    Doctor {
        /// Remove orphaned sandcrab containers (host-wide: also those of other running
        /// sandcrab processes) and tighten state dir permissions.
        #[arg(long)]
        fix: bool,
    },

    /// Manage persistent workspaces.
    Workspace {
        #[command(subcommand)]
        action: workspace::WorkspaceCmd,
    },

    /// List or build templates.
    Template {
        #[command(subcommand)]
        action: workspace::TemplateCmd,
    },

    /// Spawn a sandbox, run a command, print its output, destroy the sandbox.
    ///
    /// One trailing argument is run as a shell script (`-- 'ls | wc -l'`); several are
    /// quoted as an argv (`-- bash -c 'exit 3'`). Exits with the command's exit code.
    Run {
        #[command(flatten)]
        spawn: SpawnArgs,

        /// After the command finishes, keep the sandbox (and its published ports) alive
        /// until Ctrl-C, then destroy it.
        #[arg(long)]
        keep: bool,

        /// Command to run (after `--`).
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },

    /// Spawn a sandbox, make one tool call, print the JSON result, destroy the sandbox.
    Tool {
        #[command(flatten)]
        spawn: SpawnArgs,

        /// Tool name (see `sandcrab tools schema`).
        name: String,

        /// JSON params object (`-` reads stdin). Default `{}`.
        params: Option<String>,
    },

    /// Tool catalog.
    Tools {
        #[command(subcommand)]
        action: ToolsCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum ToolsCmd {
    /// Print the LLM tool definitions (name, description, input/output JSON Schema).
    Schema,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = match cli.global.verbose {
        0 | 1 => "warn",
        2 => "info,sandcrab=debug",
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
    match rt.block_on(dispatch(cli)) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli) -> anyhow::Result<u8> {
    let g = &cli.global;
    match cli.command {
        Commands::Doctor { fix } => doctor::doctor(g, fix).await,
        Commands::Workspace { action } => workspace::workspace(g, action),
        Commands::Template { action } => workspace::template(g, action),
        Commands::Run {
            spawn,
            keep,
            command,
        } => run::run(g, &spawn, keep, &command).await,
        Commands::Tool {
            spawn,
            name,
            params,
        } => run::tool(g, &spawn, &name, params.as_deref()).await,
        Commands::Tools {
            action: ToolsCmd::Schema,
        } => {
            util::print_json(&sandcrab_sdk::tool_definitions())?;
            Ok(0)
        }
    }
}
