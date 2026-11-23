//! `sandcrab` CLI — operator & developer interface for Sandcrab.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "sandcrab",
    version,
    about = "Rust-based ephemeral sandbox manager for AI agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Inspect local host environment (KVM/Firecracker, Cloud Hypervisor, libkrun, Docker, Rust).
    Doctor,

    /// Manage persistent workspaces that can be mounted into ephemeral sandboxes.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceCommands,
    },

    /// List or build OCI templates.
    Template {
        #[command(subcommand)]
        action: TemplateCommands,
    },

    /// Spin up an ephemeral sandbox, run a command, and tear down.
    Run {
        /// Template name (e.g. base, python-dev, ts-dev, render).
        #[arg(long, default_value = "base")]
        template: String,

        /// Mount workspace specs in `<workspace>:<guest_path>[:rw|ro]` format.
        #[arg(long = "mount")]
        mounts: Vec<String>,

        /// Default working directory inside the sandbox.
        #[arg(long)]
        cwd: Option<PathBuf>,

        /// Command to execute via `bash`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommands {
    Create { name: String },
    Register { name: String, path: PathBuf },
    List,
    Rm { name: String },
}

#[derive(Debug, Subcommand)]
enum TemplateCommands {
    List,
    Build { name: String },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Doctor => {
            println!("sandcrab doctor: environment checks pending Phase 1E implementation");
            println!(
                "registered protocol tools: {}",
                sandcrab_protocol::tool_definitions().len()
            );
        }
        Commands::Workspace { action } => {
            println!("sandcrab workspace ({action:?}): pending Phase 1C/1E implementation");
        }
        Commands::Template { action } => {
            println!("sandcrab template ({action:?}): pending Phase 1C/1E implementation");
        }
        Commands::Run {
            template,
            mounts,
            cwd,
            command,
        } => {
            println!(
                "sandcrab run (template={template}, mounts={mounts:?}, cwd={cwd:?}, cmd={command:?}): pending Phase 1D/1E implementation"
            );
        }
    }
}
