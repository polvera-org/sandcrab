//! A crabagent session: resolve workspaces, spawn the sandbox, run the REPL or a one-shot
//! task, tear everything down.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use sandcrab_sdk::{
    MountMode, Sandbox, SandboxConfig, SandboxError, Sandcrab, WorkspaceBacking, WorkspaceClient,
    WorkspaceMount, WorkspaceRef,
};
use serde_json::json;

use crate::agent::{Agent, Interrupt, TurnOutcome};
use crate::cli::{BackendArg, Cli, HostMount, host_workspace_name, truthy};
use crate::llm::LlmClient;
use crate::llm::anthropic::{AnthropicClient, AnthropicConfig};
use crate::prompt::{MountInfo, PromptContext, system_prompt};
use crate::tools::anthropic_tools;
use crate::transcript::{self, Transcript};
use crate::ui::Ui;

pub const INSECURE_ENV: &str = "SANDCRAB_ALLOW_INSECURE";
pub const SSH_HOST_ENV: &str = "CRABAGENT_SSH_HOST";
pub const DEFAULT_GUEST_WORKSPACE: &str = "/workspace";

/// Values from a `.env` file (never exported into the process environment).
///
/// `Debug` prints the keys only: values hold secrets such as `ANTHROPIC_API_KEY`.
#[derive(Default, Clone)]
pub struct DotEnv {
    pub path: Option<PathBuf>,
    vars: Vec<(String, String)>,
}

impl std::fmt::Debug for DotEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DotEnv")
            .field("path", &self.path)
            .field(
                "keys",
                &self
                    .vars
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl DotEnv {
    /// Load `explicit`, else the first `.env` from `start` upwards, stopping at the
    /// repository root (a directory containing `.git`).
    pub fn load(explicit: Option<&Path>, start: &Path) -> anyhow::Result<Self> {
        let path = match explicit {
            Some(p) => Some(p.to_owned()),
            None => find_dotenv(start),
        };
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let vars = dotenvy::from_path_iter(&path)
            .with_context(|| format!("reading {}", path.display()))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Self {
            path: Some(path),
            vars,
        })
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<String> {
        self.vars
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    /// Process environment first, then the file. Empty values count as unset.
    #[must_use]
    pub fn lookup(&self, key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| self.get(key).filter(|v| !v.trim().is_empty()))
    }
}

#[must_use]
pub fn find_dotenv(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let p = dir.join(".env");
        if p.is_file() {
            return Some(p);
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    None
}

/// Everything a session needs besides the backend and the model.
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
    pub workspace: Option<String>,
    pub template: String,
    pub mounts: Vec<WorkspaceMount>,
    pub host_mounts: Vec<HostMount>,
    pub cwd: Option<String>,
    pub network: Option<sandcrab_sdk::NetworkPolicy>,
    pub ports: Vec<u16>,
    pub env: Vec<(String, String)>,
    pub max_turns: u32,
    pub transcript: Option<PathBuf>,
    /// One-shot task; `None` = REPL.
    pub prompt: Option<String>,
    /// `user@host` for the port-forward hint.
    pub ssh_host: Option<String>,
    pub quiet: bool,
}

impl SessionOptions {
    #[must_use]
    pub fn from_cli(cli: &Cli) -> Self {
        Self {
            workspace: cli.workspace.clone(),
            template: cli.template.clone(),
            mounts: cli.mounts.clone(),
            host_mounts: cli.mount_hosts.clone(),
            cwd: cli.cwd.clone(),
            network: cli.network,
            ports: cli.ports.clone(),
            env: cli.env.clone(),
            max_turns: cli.max_turns,
            transcript: cli.transcript.clone(),
            prompt: cli.prompt.clone(),
            ssh_host: None,
            quiet: false,
        }
    }
}

/// The result of a finished session.
#[derive(Debug, Clone)]
pub struct SessionReport {
    pub exit_code: u8,
    pub outcome: Option<TurnOutcome>,
    pub transcript: Option<PathBuf>,
    pub usage_summary: String,
    pub sandbox_id: String,
}

/// Register `host` as a workspace (or reuse the existing registration of the same
/// canonical directory); returns its name. A directory inside or containing another
/// workspace is refused (nested workspaces are unsafe) with a hint to mount the existing
/// workspace instead.
pub fn ensure_host_workspace(ws: &WorkspaceClient, host: &Path) -> anyhow::Result<String> {
    let canon = std::fs::canonicalize(host)
        .with_context(|| format!("--mount-host: cannot resolve {}", host.display()))?;
    let all = ws.list()?;
    if let Some(w) = all
        .iter()
        .find(|w| matches!(&w.backing, WorkspaceBacking::HostDir(p) if *p == canon))
    {
        return Ok(w.name.clone());
    }
    for w in all.iter().filter(|w| w.backing.is_dir()) {
        let p = w.backing.path();
        if let Ok(sub) = canon.strip_prefix(p) {
            bail!(
                "--mount-host {}: it is inside workspace `{}` ({}); nested workspaces are \
                 refused. Mount the parent instead, e.g. `--mount {}:/parent:ro`, and use \
                 `/parent/{}` in the sandbox",
                canon.display(),
                w.name,
                p.display(),
                w.name,
                sub.display()
            );
        }
        if p.starts_with(&canon) {
            bail!(
                "--mount-host {}: it contains workspace `{}` ({}); nested workspaces are \
                 refused. Mount `{}` with `--mount {}:<guest_path>` instead, or remove it \
                 first with `sandcrab workspace rm {}`",
                canon.display(),
                w.name,
                p.display(),
                w.name,
                w.name,
                w.name
            );
        }
    }
    let taken: HashSet<&str> = all.iter().map(|w| w.name.as_str()).collect();
    let base = host_workspace_name(&canon);
    let mut name = base.clone();
    let mut n = 2;
    while taken.contains(name.as_str()) {
        name = format!("{base}-{n}");
        n += 1;
    }
    ws.register_host_dir(&name, &canon)
        .with_context(|| format!("registering {} as workspace `{name}`", canon.display()))?;
    Ok(name)
}

/// Create the named workspace if it does not exist. Returns `true` if created.
pub fn ensure_workspace(ws: &WorkspaceClient, name: &str) -> anyhow::Result<bool> {
    match ws.get(WorkspaceRef::parse(name)) {
        Ok(_) => Ok(false),
        Err(SandboxError::NotFound { .. }) => {
            ws.create(name)
                .with_context(|| format!("creating workspace `{name}`"))?;
            Ok(true)
        }
        Err(e) => Err(e.into()),
    }
}

fn describe_workspace(ws: &WorkspaceClient, r: &WorkspaceRef) -> String {
    match ws.get(r.clone()) {
        Ok(w) => match &w.backing {
            WorkspaceBacking::HostDir(p) => {
                format!("host directory {} (workspace `{}`)", p.display(), w.name)
            }
            _ => format!("persistent workspace `{}`", w.name),
        },
        Err(_) => format!("workspace {r}"),
    }
}

/// Run a whole session on `sc` with `llm`. Destroys the sandbox before returning.
pub async fn run_session(
    sc: &Sandcrab,
    llm: Arc<dyn LlmClient>,
    opts: SessionOptions,
    intr: &Interrupt,
) -> anyhow::Result<SessionReport> {
    let mut ui = if opts.quiet {
        Ui::quiet()
    } else {
        Ui::default()
    };
    let ws = sc.workspaces();

    // 1. Mounts.
    let mut mounts = Vec::new();
    let mut infos: Vec<MountInfo> = Vec::new();
    let mut cwd = opts.cwd.clone();
    if let Some(name) = &opts.workspace {
        let created = ensure_workspace(&ws, name)?;
        let guest = opts
            .cwd
            .clone()
            .unwrap_or_else(|| DEFAULT_GUEST_WORKSPACE.to_owned());
        cwd.get_or_insert_with(|| guest.clone());
        mounts.push(WorkspaceMount {
            workspace: WorkspaceRef::parse(name),
            guest_path: PathBuf::from(&guest),
            mode: MountMode::ReadWrite,
        });
        if created {
            ui.info(&format!("created workspace `{name}`"));
        }
    }
    mounts.extend(opts.mounts.iter().cloned());
    for h in &opts.host_mounts {
        let name = ensure_host_workspace(&ws, &h.host)?;
        mounts.push(WorkspaceMount {
            workspace: WorkspaceRef::parse(&name),
            guest_path: h.guest.clone(),
            mode: h.mode,
        });
    }
    for m in &mounts {
        infos.push(MountInfo {
            guest_path: m.guest_path.display().to_string(),
            read_only: m.mode == MountMode::ReadOnly,
            description: describe_workspace(&ws, &m.workspace),
        });
    }
    if cwd.is_none() {
        cwd = mounts
            .iter()
            .find(|m| m.mode == MountMode::ReadWrite)
            .map(|m| m.guest_path.display().to_string());
    }

    // 2. Spawn.
    let mut config = SandboxConfig::from_template(&opts.template);
    config.mounts.clone_from(&mounts);
    config.default_cwd = cwd.as_ref().map(PathBuf::from);
    config.env.extend(opts.env.iter().cloned());
    if let Some(n) = opts.network {
        config = config.network(n);
    }
    for p in &opts.ports {
        config = config.port(*p);
    }
    let network = match opts.network {
        Some(n) => n.to_string(),
        None => sc
            .templates()
            .get(&opts.template)
            .map(|t| t.manifest.network.default_policy.to_string())
            .unwrap_or_else(|_| "template default".into()),
    };
    if !sc.backend_capabilities().hardware_isolated {
        #[cfg(feature = "docker")]
        let disk = sc
            .docker_backend()
            .is_some_and(|d| !d.enforces_disk_limit());
        #[cfg(not(feature = "docker"))]
        let disk = false;
        ui.info(&format!(
            "warning: backend `{}` shares the host kernel — not a security boundary; local dev / CI only{}",
            sc.backend_kind(),
            if disk {
                "; template disk_mb is not enforced"
            } else {
                ""
            }
        ));
    }
    ui.info(&format!("spawning `{}` sandbox…", opts.template));
    let sandbox = sc
        .spawn(config)
        .await
        .with_context(|| format!("spawning a `{}` sandbox", opts.template))?;
    let sandbox = Arc::new(sandbox);
    let sandbox_id = sandbox.id().to_string();

    let state_root = sc.state_dir().to_path_buf();
    let result = drive(
        &sandbox,
        llm,
        &opts,
        &infos,
        &network,
        &state_root,
        intr,
        &mut ui,
    )
    .await;

    // 3. Teardown (always).
    match Arc::try_unwrap(sandbox) {
        Ok(sb) => {
            if let Err(e) = sb.destroy().await {
                ui.line(&format!("warning: destroying sandbox {sandbox_id}: {e}"));
            } else {
                ui.info(&format!("sandbox {sandbox_id} destroyed"));
            }
        }
        Err(_still_shared) => {
            // A tool task still holds it; dropping triggers a background destroy and the
            // caller's `Sandcrab::shutdown` is the final safety net.
        }
    }
    let mut report = result?;
    report.sandbox_id = sandbox_id;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    sandbox: &Arc<Sandbox>,
    llm: Arc<dyn LlmClient>,
    opts: &SessionOptions,
    infos: &[MountInfo],
    network: &str,
    state_root: &Path,
    intr: &Interrupt,
    ui: &mut Ui,
) -> anyhow::Result<SessionReport> {
    let session = transcript::new_session_id();
    let tpath = match &opts.transcript {
        Some(p) => p.clone(),
        None => transcript::default_path(state_root, &session),
    };
    let transcript = Transcript::open(&tpath)
        .map_err(|e| {
            ui.line(&format!(
                "warning: no transcript ({}): {e}",
                tpath.display()
            ))
        })
        .ok();

    // Banner.
    let cwd = sandbox.default_cwd().display().to_string();
    ui.line(&format!(
        "{} {}  template {}  backend {}  model {}",
        ui.bold("🦀 sandbox"),
        sandbox.id(),
        sandbox.template(),
        sandbox.backend(),
        llm.model()
    ));
    ui.info(&format!("   cwd {cwd}  network {network}"));
    for m in infos {
        ui.info(&format!(
            "   mount {} ({}) — {}",
            m.guest_path,
            if m.read_only { "ro" } else { "rw" },
            m.description
        ));
    }
    let mut ports = Vec::new();
    for p in sandbox.published_ports() {
        ports.push((p.guest, p.host.to_string()));
        ui.line(&format!(
            "   port {} → {}  (http://{})",
            p.host, p.guest, p.host
        ));
        if let Some(h) = &opts.ssh_host {
            let hp = p.host.port();
            ui.info(&format!("     tunnel: ssh -L {hp}:127.0.0.1:{hp} {h}"));
        }
    }
    if let Some(t) = &transcript {
        ui.info(&format!("   transcript {}", t.path().display()));
    }

    let defs = sandbox.tool_definitions();
    let tools = anthropic_tools(&defs);
    let system = system_prompt(&PromptContext {
        template: sandbox.template().to_owned(),
        cwd: cwd.clone(),
        mounts: infos.to_vec(),
        network: network.to_owned(),
        ports: ports.clone(),
        tools: defs.iter().map(|d| d.name.to_owned()).collect(),
    });

    let mut agent = Agent::new(llm.clone(), sandbox.clone(), system, tools).with_ui(ui.clone());
    agent.max_turns = opts.max_turns.max(1);
    if let Some(t) = transcript {
        agent = agent.with_transcript(t);
    }
    agent.log(
        "session_start",
        json!({
            "session": session,
            "sandbox": sandbox.id().to_string(),
            "template": sandbox.template(),
            "backend": sandbox.backend().to_string(),
            "model": llm.model(),
            "cwd": cwd,
            "network": network,
            "mounts": infos.iter().map(|m| json!({"guest": m.guest_path, "read_only": m.read_only, "desc": m.description})).collect::<Vec<_>>(),
            "ports": ports.iter().map(|(g, h)| json!({"guest": g, "host": h})).collect::<Vec<_>>(),
            "system": agent.system(),
            "tools": defs.iter().map(|d| d.name).collect::<Vec<_>>(),
        }),
    );

    let (exit_code, outcome) = if intr.count() > 0 {
        (130, Some(TurnOutcome::Interrupted))
    } else if let Some(prompt) = &opts.prompt {
        let out = agent.run(prompt, intr).await;
        let code = match out {
            TurnOutcome::Done => 0,
            TurnOutcome::Interrupted => 130,
            _ => 1,
        };
        (code, Some(out))
    } else {
        repl(
            &mut agent,
            intr,
            sandbox,
            &state_root.join("crabagent_history"),
        )
        .await?;
        (0, None)
    };

    let usage_summary = agent.usage.summary();
    agent.log(
        "session_end",
        json!({
            "exit_code": exit_code,
            "outcome": outcome.as_ref().map(|o| format!("{o:?}")),
            "usage": agent.usage.by_model,
            "requests": agent.usage.requests,
            "cost_usd": agent.usage.cost().0,
        }),
    );
    let tpath = agent.transcript_path().map(Path::to_path_buf);
    ui.info(&format!("usage: {usage_summary}"));
    if let Some(p) = &tpath {
        ui.info(&format!("transcript: {}", p.display()));
    }
    Ok(SessionReport {
        exit_code,
        outcome,
        transcript: tpath,
        usage_summary,
        sandbox_id: String::new(),
    })
}

const HELP: &str = "/exit  quit (destroys the sandbox; the workspace persists)\n\
                    /reset  forget the conversation (same sandbox)\n\
                    /ports  published ports\n\
                    /cost   token usage and cost so far\n\
                    Ctrl-C during a turn interrupts it; Ctrl-D quits.";

async fn repl(
    agent: &mut Agent,
    intr: &Interrupt,
    sandbox: &Sandbox,
    history: &Path,
) -> anyhow::Result<()> {
    use rustyline::error::ReadlineError;

    let mut ed = rustyline::DefaultEditor::new().context("starting the line editor")?;
    let _ = ed.load_history(history);
    agent.ui.info("type a task, /help for commands");
    loop {
        let (e, line) = tokio::task::spawn_blocking(move || {
            let r = ed.readline("\ncrab› ");
            (ed, r)
        })
        .await
        .context("line editor task")?;
        ed = e;
        let line = match line {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let _ = ed.add_history_entry(line);
        match line {
            "/exit" | "/quit" | "/q" => break,
            "/help" | "/?" => agent.ui.info(HELP),
            "/reset" => {
                agent.reset();
                agent.ui.info("conversation cleared");
            }
            "/cost" => {
                let s = agent.usage.summary();
                agent.ui.info(&s);
            }
            "/ports" => {
                if sandbox.published_ports().is_empty() {
                    agent.ui.info("no published ports (use --port)");
                }
                for p in sandbox.published_ports() {
                    agent
                        .ui
                        .info(&format!("{} → {}  (http://{})", p.host, p.guest, p.host));
                }
            }
            cmd if cmd.starts_with('/') => agent.ui.info(&format!("unknown command {cmd}; /help")),
            task => {
                if let TurnOutcome::SandboxLost(_) = agent.run(task, intr).await {
                    break;
                }
            }
        }
    }
    let _ = ed.save_history(history);
    Ok(())
}

/// Build a [`Sandcrab`] on the explicitly selected backend.
pub fn build_sandcrab(cli: &Cli) -> anyhow::Result<Sandcrab> {
    let Some(backend) = cli.backend else {
        bail!(
            "no backend selected: pass --backend docker (or set SANDCRAB_BACKEND); crabagent never picks one"
        );
    };
    let state = match &cli.state_dir {
        Some(d) => d.clone(),
        None => sandcrab_sdk::core::default_state_root()?,
    };
    let mut builder = Sandcrab::builder().state_dir(&state);
    let dirs = if cli.templates_dirs.is_empty() {
        [PathBuf::from("templates"), state.join("templates")]
            .into_iter()
            .filter(|d| d.is_dir())
            .collect()
    } else {
        cli.templates_dirs.clone()
    };
    for d in dirs {
        builder = builder.templates_dir(d);
    }
    match backend {
        #[cfg(feature = "docker")]
        BackendArg::Docker => {
            let allow = cli.allow_insecure || std::env::var(INSECURE_ENV).is_ok_and(|v| truthy(&v));
            if !allow {
                bail!(
                    "the docker backend shares the host kernel and is for local dev / CI only; \
                     pass --allow-insecure or set {INSECURE_ENV}=1"
                );
            }
            builder = builder.docker(sandcrab_sdk::DockerDevConfig {
                crabd_path: cli
                    .crabd
                    .clone()
                    .unwrap_or_else(sandcrab_sdk::docker::default_crabd_path),
                ..sandcrab_sdk::DockerDevConfig::allow_insecure()
            });
        }
    }
    builder.build().context("cannot initialise the backend")
}

fn default_ssh_host(dot: &DotEnv) -> Option<String> {
    if let Some(h) = dot.lookup(SSH_HOST_ENV) {
        return Some(h);
    }
    let user = std::env::var("USER").ok()?;
    let host = std::fs::read_to_string("/etc/hostname").ok()?;
    let host = host.trim();
    (!host.is_empty()).then(|| format!("{user}@{host}"))
}

/// The `crabagent` command.
pub async fn main(cli: Cli) -> anyhow::Result<u8> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let dot = DotEnv::load(cli.env_file.as_deref(), &cwd)?;
    let Some(api_key) = dot.lookup("ANTHROPIC_API_KEY") else {
        bail!(
            "ANTHROPIC_API_KEY is not set: export it or put it in a .env file{}",
            dot.path
                .as_ref()
                .map(|p| format!(" (looked in {})", p.display()))
                .unwrap_or_default()
        );
    };
    let mut cfg = AnthropicConfig::new(api_key, &cli.model);
    cfg.max_tokens = cli.max_tokens;
    cfg.effort.clone_from(&cli.effort);
    cfg.fallback = !cli.no_fallback;
    if let Some(url) = dot.lookup("ANTHROPIC_BASE_URL") {
        cfg.base_url = url;
    }
    let llm: Arc<dyn LlmClient> = Arc::new(AnthropicClient::new(cfg)?);

    let mut opts = SessionOptions::from_cli(&cli);
    opts.ssh_host = default_ssh_host(&dot);
    if opts.prompt.as_deref() == Some("-") {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading the task from stdin")?;
        opts.prompt = Some(s);
    }

    let sc = Arc::new(build_sandcrab(&cli)?);
    let intr = Interrupt::install();
    let mut task = {
        let sc = sc.clone();
        let intr = intr.clone();
        tokio::spawn(async move { run_session(&sc, llm, opts, &intr).await })
    };
    let joined = tokio::select! {
        j = &mut task => j,
        sig = intr.terminated() => {
            // SIGTERM / SIGHUP / SIGQUIT: destroy every sandbox, then exit. Don't wait for
            // the session task: the REPL may be blocked reading a (possibly gone) terminal.
            eprintln!("crabagent: received signal {sig}; destroying the sandbox");
            if let Err(e) = sc.shutdown().await {
                eprintln!("warning: sandbox cleanup: {e}");
            }
            std::process::exit(128 + sig);
        }
    };
    // Safety net: whatever happened above (error, panic), no sandbox outlives us.
    if let Err(e) = sc.shutdown().await {
        eprintln!("warning: sandbox cleanup: {e}");
    }
    match joined {
        Ok(Ok(report)) => Ok(report.exit_code),
        Ok(Err(e)) => Err(e),
        Err(e) => bail!("crabagent crashed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_host_reuses_and_refuses_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let ws = WorkspaceClient::open(dir.path().join("state")).unwrap();
        let outer = dir.path().join("outer");
        std::fs::create_dir_all(outer.join("inner")).unwrap();
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        let a = ensure_host_workspace(&ws, &outer).unwrap();
        // Same canonical dir (through `.`): reused, not re-registered.
        assert_eq!(ensure_host_workspace(&ws, &outer.join(".")).unwrap(), a);
        let e = ensure_host_workspace(&ws, &outer.join("inner"))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("inside workspace") && e.contains("/parent/inner"),
            "{e}"
        );
        let e = ensure_host_workspace(&ws, dir.path())
            .unwrap_err()
            .to_string();
        assert!(e.contains("contains workspace"), "{e}");
        ensure_host_workspace(&ws, &dir.path().join("other")).unwrap();
        assert_eq!(ws.list().unwrap().len(), 2);
    }

    #[test]
    fn dotenv_lookup_and_discovery() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let sub = dir.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "CRABAGENT_TEST_ONLY_KEY=from-file\nEMPTY=\n",
        )
        .unwrap();
        assert_eq!(find_dotenv(&sub), Some(dir.path().join(".env")));
        let d = DotEnv::load(None, &sub).unwrap();
        assert_eq!(
            d.get("CRABAGENT_TEST_ONLY_KEY").as_deref(),
            Some("from-file")
        );
        assert_eq!(
            d.lookup("CRABAGENT_TEST_ONLY_KEY").as_deref(),
            Some("from-file")
        );
        assert!(d.lookup("EMPTY").is_none());
        // .env above the repo root is not used
        let inner = tempfile::tempdir().unwrap();
        std::fs::create_dir(inner.path().join(".git")).unwrap();
        assert!(find_dotenv(inner.path()).is_none());
    }

    #[test]
    fn dotenv_debug_redacts_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "ANTHROPIC_API_KEY=sk-ant-very-secret-value\nOTHER=hunter2\n",
        )
        .unwrap();
        let d = DotEnv::load(Some(&path), dir.path()).unwrap();
        let dbg = format!("{d:?}");
        assert!(
            dbg.contains("ANTHROPIC_API_KEY") && dbg.contains("OTHER"),
            "{dbg}"
        );
        assert!(!dbg.contains("sk-ant-very-secret-value"), "{dbg}");
        assert!(!dbg.contains("hunter2"), "{dbg}");
        let alt = format!("{d:#?}");
        assert!(!alt.contains("sk-ant") && !alt.contains("hunter2"), "{alt}");
    }
}
