//! Command-line interface and argument parsers.

use std::path::{Path, PathBuf};

use clap::Parser;
use sandcrab_sdk::{MountMode, NetworkPolicy, WorkspaceMount};

use crate::llm::anthropic::{DEFAULT_MAX_TOKENS, DEFAULT_MODEL};

/// Runtime backends compiled into this binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendArg {
    /// Docker: shared kernel, local dev / CI only.
    #[cfg(feature = "docker")]
    Docker,
}

pub fn parse_backend(s: &str) -> Result<BackendArg, String> {
    match s {
        #[cfg(feature = "docker")]
        "docker" | "docker-dev" => Ok(BackendArg::Docker),
        "firecracker" | "cloud-hypervisor" | "libkrun" => Err(format!(
            "backend `{s}` is not available in crabagent yet (available: docker)"
        )),
        other => Err(format!("unknown backend `{other}` (available: docker)")),
    }
}

/// `<workspace>:<guest_path>[:rw|ro]`.
pub fn parse_mount(s: &str) -> Result<WorkspaceMount, String> {
    WorkspaceMount::parse_spec(s).map_err(|e| e.to_string())
}

/// A host directory to register as a workspace on the fly and mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMount {
    pub host: PathBuf,
    pub guest: PathBuf,
    pub mode: MountMode,
}

/// `<host_dir>:<guest_path>[:ro|rw]` (default **ro**).
pub fn parse_mount_host(s: &str) -> Result<HostMount, String> {
    let parts: Vec<&str> = s.split(':').collect();
    let (host, guest, mode) = match parts.as_slice() {
        [h, g] => (*h, *g, MountMode::ReadOnly),
        [h, g, "ro"] => (*h, *g, MountMode::ReadOnly),
        [h, g, "rw"] => (*h, *g, MountMode::ReadWrite),
        [_, _, m] => return Err(format!("mount mode `{m}` must be ro or rw")),
        _ => return Err(format!("`{s}` must be <host_dir>:<guest_path>[:ro|rw]")),
    };
    if host.is_empty() {
        return Err(format!("`{s}`: empty host directory"));
    }
    if !guest.starts_with('/') {
        return Err(format!("`{s}`: guest path must be absolute"));
    }
    Ok(HostMount {
        host: PathBuf::from(host),
        guest: PathBuf::from(guest),
        mode,
    })
}

/// `KEY=VALUE`.
pub fn parse_env(s: &str) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("env `{s}` must be KEY=VALUE"))?;
    if k.is_empty() {
        return Err(format!("env `{s}` has an empty name"));
    }
    Ok((k.to_owned(), v.to_owned()))
}

pub fn parse_network(s: &str) -> Result<NetworkPolicy, String> {
    s.parse()
        .map_err(|e: sandcrab_sdk::SandboxError| e.to_string())
}

/// `1`, `true`, `yes`, `on` (case-insensitive).
#[must_use]
pub fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Workspace name for a registered host dir: `host-<basename>`.
#[must_use]
pub fn host_workspace_name(dir: &Path) -> String {
    let base = dir
        .file_name()
        .map(|b| b.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let slug: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "root" } else { slug };
    format!("host-{slug}").chars().take(56).collect()
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "crabagent",
    version,
    about = "Minimal coding agent: Claude working inside a Sandcrab sandbox",
    long_about = "Minimal coding agent: Claude working inside a Sandcrab sandbox.\n\n\
        One ephemeral sandbox per session; persistent workspaces survive across sessions. \
        Without -p: interactive REPL (/exit, /reset, /ports, /cost, /help). \
        With -p: run one task to completion (exit 0 = done, 1 = not done, 130 = Ctrl-C).",
    after_help = "Example (landing page):\n  crabagent --workspace landing --template ts-dev \
        --mount-host docs/site:/kit:ro --network unrestricted --port 5173 \
        --backend docker --allow-insecure\n\n\
        ANTHROPIC_API_KEY is read from the environment or a .env file (cwd up to the repo root)."
)]
pub struct Cli {
    /// Persistent workspace mounted read-write at --cwd (default /workspace); created if
    /// missing.
    #[arg(long, short = 'w')]
    pub workspace: Option<String>,

    /// Sandbox template.
    #[arg(long, short = 't', default_value = "ts-dev")]
    pub template: String,

    /// Mount an existing workspace: `<workspace>:<guest_path>[:rw|ro]`. Repeatable.
    #[arg(long = "mount", short = 'm', value_parser = parse_mount)]
    pub mounts: Vec<WorkspaceMount>,

    /// Register a host directory as a workspace (reused if already registered) and mount
    /// it: `<host_dir>:<guest_path>[:ro|rw]` (default ro). Repeatable.
    #[arg(long = "mount-host", value_parser = parse_mount_host)]
    pub mount_hosts: Vec<HostMount>,

    /// Working directory in the sandbox (default: /workspace with --workspace, else the
    /// first rw mount, else the template workdir).
    #[arg(long)]
    pub cwd: Option<String>,

    /// Network policy: disabled | unrestricted (default: the template's).
    #[arg(long, value_parser = parse_network)]
    pub network: Option<NetworkPolicy>,

    /// Publish a guest port on host loopback. Repeatable.
    #[arg(long = "port")]
    pub ports: Vec<u16>,

    /// Sandbox environment variable `KEY=VALUE`. Repeatable.
    #[arg(long = "env", short = 'e', value_parser = parse_env)]
    pub env: Vec<(String, String)>,

    /// Runtime backend (explicit; no fallback). Available: docker.
    #[arg(long, env = "SANDCRAB_BACKEND", value_parser = parse_backend)]
    pub backend: Option<BackendArg>,

    /// Acknowledge that the docker backend shares the host kernel (dev / CI only). Also
    /// SANDCRAB_ALLOW_INSECURE=1.
    #[arg(long)]
    pub allow_insecure: bool,

    /// Sandcrab state root. Default: $SANDCRAB_HOME, else ~/.sandcrab.
    #[arg(long, env = "SANDCRAB_HOME")]
    pub state_dir: Option<PathBuf>,

    /// Template directory; repeatable, first wins. Default: ./templates, then
    /// <state root>/templates.
    #[arg(long = "templates-dir")]
    pub templates_dirs: Vec<PathBuf>,

    /// Host path of the static musl `crabd` (default: $SANDCRAB_CRABD, else
    /// target/<arch>-unknown-linux-musl/release/crabd).
    #[arg(long)]
    pub crabd: Option<PathBuf>,

    /// Claude model id.
    #[arg(long, env = "CRABAGENT_MODEL", default_value = DEFAULT_MODEL)]
    pub model: String,

    /// Effort (output_config.effort). Default: the model's default.
    #[arg(long, env = "CRABAGENT_EFFORT", value_parser = ["low", "medium", "high", "xhigh", "max"])]
    pub effort: Option<String>,

    /// max_tokens per model response.
    #[arg(long, default_value_t = DEFAULT_MAX_TOKENS)]
    pub max_tokens: u32,

    /// Model calls per user turn before giving up.
    #[arg(long, default_value_t = 100)]
    pub max_turns: u32,

    /// Do not send the server-side refusal fallback (`fallbacks: "default"`).
    #[arg(long)]
    pub no_fallback: bool,

    /// Transcript path (JSONL). Default: <state root>/transcripts/<session>.jsonl.
    #[arg(long)]
    pub transcript: Option<PathBuf>,

    /// Run one task and exit (`-` reads it from stdin). Without it: interactive REPL.
    #[arg(long, short = 'p')]
    pub prompt: Option<String>,

    /// With -p: keep the sandbox (and its published ports) up for SECS after the task
    /// finishes, so a preview URL stays reachable; Ctrl-C ends the hold early. Default 0:
    /// the sandbox is destroyed right away.
    #[arg(long, value_name = "SECS", default_value_t = 0, requires = "prompt")]
    pub keep_alive: u64,

    /// .env file to read ANTHROPIC_API_KEY (etc.) from. Default: the first .env from the
    /// current directory up to the repository root.
    #[arg(long)]
    pub env_file: Option<PathBuf>,

    /// Debug logs to stderr (-vv for more).
    #[arg(long, short = 'v', action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[cfg(test)]
mod tests {
    use sandcrab_sdk::WorkspaceRef;

    use super::*;

    #[test]
    fn mount_host_specs() {
        let m = parse_mount_host("docs/site:/kit:ro").unwrap();
        assert_eq!(m.host, PathBuf::from("docs/site"));
        assert_eq!(m.guest, PathBuf::from("/kit"));
        assert_eq!(m.mode, MountMode::ReadOnly);
        assert_eq!(parse_mount_host("a:/b").unwrap().mode, MountMode::ReadOnly);
        assert_eq!(
            parse_mount_host("a:/b:rw").unwrap().mode,
            MountMode::ReadWrite
        );
        for bad in ["a", "a:b", ":/b", "a:/b:rx", "a:/b:ro:x"] {
            assert!(parse_mount_host(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn keep_alive_needs_one_shot() {
        let c = Cli::try_parse_from(["crabagent", "-p", "go", "--keep-alive", "300"]).unwrap();
        assert_eq!(c.keep_alive, 300);
        assert_eq!(
            Cli::try_parse_from(["crabagent", "-p", "go"])
                .unwrap()
                .keep_alive,
            0
        );
        assert!(Cli::try_parse_from(["crabagent", "--keep-alive", "5"]).is_err());
    }

    #[test]
    fn host_names() {
        assert_eq!(host_workspace_name(Path::new("/x/docs/site")), "host-site");
        assert_eq!(
            host_workspace_name(Path::new("/x/My Kit.v2")),
            "host-my-kit-v2"
        );
        assert_eq!(host_workspace_name(Path::new("/")), "host-root");
        assert!(host_workspace_name(Path::new(&"a".repeat(200))).len() <= 56);
    }

    #[test]
    fn landing_page_command_parses() {
        let cli = Cli::try_parse_from([
            "crabagent",
            "--workspace",
            "landing",
            "--template",
            "ts-dev",
            "--mount-host",
            "docs/site:/kit:ro",
            "--network",
            "unrestricted",
            "--port",
            "5173",
            "--backend",
            "docker",
            "--allow-insecure",
        ])
        .unwrap();
        assert_eq!(cli.workspace.as_deref(), Some("landing"));
        assert_eq!(cli.template, "ts-dev");
        assert_eq!(cli.mount_hosts.len(), 1);
        assert_eq!(cli.network, Some(NetworkPolicy::Unrestricted));
        assert_eq!(cli.ports, [5173]);
        assert!(cli.allow_insecure);
        #[cfg(feature = "docker")]
        assert_eq!(cli.backend, Some(BackendArg::Docker));
        assert!(cli.prompt.is_none());
        assert_eq!(cli.max_turns, 100);
    }

    #[test]
    fn one_shot_and_options() {
        let cli = Cli::try_parse_from([
            "crabagent",
            "-p",
            "build it",
            "-m",
            "lib:/lib:ro",
            "--max-turns",
            "7",
            "--model",
            "claude-opus-5-5",
            "--effort",
            "high",
            "--transcript",
            "/tmp/t.jsonl",
            "-e",
            "A=1",
            "--cwd",
            "/src",
        ])
        .unwrap();
        assert_eq!(cli.prompt.as_deref(), Some("build it"));
        assert_eq!(cli.mounts[0].workspace, WorkspaceRef::parse("lib"));
        assert_eq!(cli.mounts[0].mode, MountMode::ReadOnly);
        assert_eq!(cli.max_turns, 7);
        assert_eq!(cli.model, "claude-opus-5-5");
        assert_eq!(cli.effort.as_deref(), Some("high"));
        assert_eq!(cli.env, [("A".to_owned(), "1".to_owned())]);
        assert_eq!(cli.cwd.as_deref(), Some("/src"));

        assert!(Cli::try_parse_from(["crabagent", "--effort", "extreme"]).is_err());
        assert!(Cli::try_parse_from(["crabagent", "--backend", "qemu"]).is_err());
        assert!(Cli::try_parse_from(["crabagent", "--network", "open"]).is_err());
        assert!(Cli::try_parse_from(["crabagent", "--mount-host", "x"]).is_err());
        assert!(truthy("YES") && !truthy("0"));
    }
}
