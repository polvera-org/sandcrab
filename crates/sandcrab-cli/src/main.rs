//! `sandcrab` — operator & developer CLI for Sandcrab.
//!
//! Exit codes: `run` exits with the sandboxed command's code (`128 + signal` if killed);
//! if `sandcrab` itself gets SIGINT / SIGTERM / SIGHUP / SIGQUIT, `run` and `tool` destroy
//! the sandbox first and exit `128 + signal` (130 on Ctrl-C); other commands exit `0` on success, `1` on error (`doctor`: any failed
//! check), `2` on usage errors.

mod args;
mod doctor;
#[cfg(all(feature = "firecracker", target_os = "linux"))]
mod firecracker;
#[cfg(all(
    feature = "libkrun",
    any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
))]
mod libkrun;
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
    after_help = "Docker (`--backend docker`) runs hardened containers on the shared host \
                  kernel: fast, officially supported for single-tenant or trusted-host \
                  workloads (not hardware isolated).\n\
                  Firecracker (`--backend firecracker`, Linux) is hardware isolated and needs a \
                  read-writable /dev/kvm. libkrun (`--backend libkrun`, Apple Silicon macOS and \
                  Linux) is hardware isolated and needs `sandcrab libkrun install`. \
                  `sandcrab doctor` checks every backend's prerequisites. A backend that cannot \
                  start is an error: sandcrab never falls back to another one."
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

    /// Runtime backend (explicit; no fallback): docker | firecracker (Linux builds) |
    /// libkrun (macOS aarch64 and Linux builds).
    #[arg(long, global = true, env = "SANDCRAB_BACKEND", value_parser = parse_backend)]
    pub backend: Option<BackendArg>,

    /// Deprecated no-op (the docker backend needs no opt-in any more); prints a warning.
    /// Also SANDCRAB_ALLOW_INSECURE. Kept hidden for one release.
    #[arg(long, global = true, hide = true)]
    pub allow_insecure: bool,

    /// docker backend: run template images that have no host-local pin (refused by default;
    /// pin with `templates/build.sh <name>` or adopt with `sandcrab template pin <name>`).
    #[arg(long, global = true)]
    pub allow_unpinned: bool,

    /// Host path of the static musl `crabd` (default: $SANDCRAB_CRABD, else
    /// <target dir>/<arch>-unknown-linux-musl/release/crabd, target dir $CARGO_TARGET_DIR or target/).
    #[arg(long, global = true)]
    pub crabd: Option<PathBuf>,

    /// DEPRECATED, libkrun only: run `--network unrestricted` through libkrun's TSI socket
    /// proxy instead of the pinned userspace netstack (passt / gvproxy, installed by
    /// `sandcrab libkrun install`). SECURITY: with TSI the guest can reach services listening
    /// on the HOST's loopback (127.0.0.1) and LAN. Off by default; never used implicitly.
    #[arg(long, global = true)]
    pub libkrun_tsi: bool,

    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    pub json: bool,

    /// Print lifecycle timings to stderr (-vv: debug logs).
    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Check the host: crabd, Docker, template images + pins, Firecracker prerequisites
    /// (/dev/kvm, pinned artifacts, rootfs cache, tap pool), libkrun prerequisites (hypervisor,
    /// signed helper, pinned libraries, mke2fs, workspace case sensitivity; reaps orphaned
    /// libkrun run dirs of dead processes), state dir.
    Doctor {
        /// Remove orphaned sandcrab containers — only this user's, for this state dir, whose
        /// sandcrab process is gone — and Firecracker run dirs of dead sandcrab processes,
        /// download + verify the pinned Firecracker kernel and binary, tighten state dir
        /// permissions.
        #[arg(long)]
        fix: bool,

        /// With --fix: remove EVERY sandcrab docker container on the host, including live
        /// sandboxes of other processes and users (asks for confirmation).
        #[arg(long, requires = "fix")]
        all: bool,

        /// With --all: skip the confirmation prompt.
        #[arg(long, short = 'y', requires = "all")]
        yes: bool,
    },

    /// Manage persistent workspaces.
    Workspace {
        #[command(subcommand)]
        action: workspace::WorkspaceCmd,
    },

    /// List, build or pin templates (pins are host-local integrity checks, not provenance).
    Template {
        #[command(subcommand)]
        action: workspace::TemplateCmd,
    },

    /// Firecracker rootfs images (ext4) built from templates.
    #[cfg(all(feature = "firecracker", target_os = "linux"))]
    Rootfs {
        #[command(subcommand)]
        action: firecracker::RootfsCmd,
    },

    /// libkrun backend: install the pinned libraries and the signed helper.
    #[cfg(all(
        feature = "libkrun",
        any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
    ))]
    Libkrun {
        #[command(subcommand)]
        action: libkrun::LibkrunCmd,
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
    // The per-spawn "disk_mb not enforced" warning is printed by our own one-liner.
    let filter = match cli.global.verbose {
        0 | 1 => "warn,sandcrab_backend_docker::disk_limit=off",
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
    util::warn_deprecated_opt_in(g);
    match cli.command {
        Commands::Doctor { fix, all, yes } => {
            let reap = match (fix, all) {
                (false, _) => doctor::Reap::None,
                (true, false) => doctor::Reap::Owned,
                (true, true) => doctor::Reap::All { confirmed: yes },
            };
            doctor::doctor(g, fix, reap).await
        }
        Commands::Workspace { action } => workspace::workspace(g, action),
        Commands::Template { action } => workspace::template(g, action).await,
        #[cfg(all(feature = "firecracker", target_os = "linux"))]
        Commands::Rootfs { action } => firecracker::rootfs_cmd(g, action),
        #[cfg(all(
            feature = "libkrun",
            any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
        ))]
        Commands::Libkrun { action } => libkrun::libkrun_cmd(g, action),
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
