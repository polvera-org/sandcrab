//! `sandcrab server`: the host daemon (`sandcrab-server`) on one explicitly chosen backend.
//!
//! The config file is read first; flags (and the global options with their env vars:
//! `--backend`, `--state-dir`, `--templates-dir`) override it.

use std::path::PathBuf;

use anyhow::Context;
use sandcrab_server::{MountRootConfig, Server, ServerConfig};

use crate::Global;
use crate::args::parse_backend;
use crate::util::{self, Interrupt};

#[derive(Debug, Clone, clap::Args)]
pub struct ServerArgs {
    /// TOML config file (keys: socket, socket_mode, socket_group, allow_uids, backend,
    /// state_dir, templates_dirs, default_template, allow_root_user, [limits], [[mount_roots]]).
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Unix socket path. Its directory holds the socket and nothing else.
    #[arg(long)]
    pub socket: Option<PathBuf>,

    /// Socket mode, octal (default 0600).
    #[arg(long)]
    pub socket_mode: Option<String>,

    /// Socket group (name or gid), for a client in another uid with mode 0660.
    #[arg(long)]
    pub socket_group: Option<String>,

    /// A peer uid to admit; repeatable, replaces `allow_uids`. Default: the server's own uid.
    #[arg(long = "allow-uid")]
    pub allow_uids: Vec<u32>,

    /// An allowed mount root, `PATH[:ro|rw]`; repeatable, replaces `[[mount_roots]]`.
    #[arg(long = "mount-root")]
    pub mount_roots: Vec<MountRootConfig>,

    /// Template used when a create names none.
    #[arg(long)]
    pub default_template: Option<String>,

    /// Sandboxes alive at once, for all clients (default 64).
    #[arg(long)]
    pub max_sandboxes: Option<u32>,

    /// Allow uid or gid 0 as a sandbox user.
    #[arg(long)]
    pub allow_root_user: bool,

    /// Stop (and destroy every sandbox) when stdin reaches end-of-file: for a server started
    /// as a child process by its client.
    #[arg(long)]
    pub lifeline_stdin: bool,
}

/// The file config with every flag applied.
fn config(g: &Global, a: &ServerArgs) -> anyhow::Result<ServerConfig> {
    let mut c = match &a.config {
        Some(p) => ServerConfig::load(p)?,
        None => ServerConfig::default(),
    };
    if let Some(s) = &a.socket {
        c.socket = Some(std::path::absolute(s)?);
    }
    if let Some(m) = &a.socket_mode {
        c.socket_mode = sandcrab_server::config::parse_mode(m).map_err(anyhow::Error::msg)?;
    }
    if a.socket_group.is_some() {
        c.socket_group.clone_from(&a.socket_group);
    }
    if !a.allow_uids.is_empty() {
        c.allow_uids = Some(a.allow_uids.clone());
    }
    if !a.mount_roots.is_empty() {
        c.mount_roots.clone_from(&a.mount_roots);
    }
    if a.default_template.is_some() {
        c.default_template.clone_from(&a.default_template);
    }
    if let Some(n) = a.max_sandboxes {
        c.limits.max_sandboxes = n;
    }
    c.allow_root_user |= a.allow_root_user;
    if let Some(d) = &g.state_dir {
        c.state_dir = Some(d.clone());
    }
    if !g.templates_dirs.is_empty() {
        c.templates_dirs.clone_from(&g.templates_dirs);
    }
    Ok(c)
}

pub async fn server(g: &Global, a: &ServerArgs) -> anyhow::Result<u8> {
    let mut c = config(g, a)?;
    // The backend: the flag, else the file. Never a default.
    let backend = match (g.backend, c.backend) {
        (Some(b), _) => b,
        (None, Some(name)) => parse_backend(name.as_str()).map_err(anyhow::Error::msg)?,
        (None, None) => anyhow::bail!(
            "no backend: set `backend` in the config or pass --backend; the server never \
             picks one"
        ),
    };
    let mut g = g.clone();
    g.backend = Some(backend);
    if let Some(d) = &c.state_dir {
        g.state_dir = Some(d.clone());
    }
    let state_root = util::state_root(&g)?;
    c.state_dir = Some(state_root.clone());
    if c.templates_dirs.is_empty() {
        c.templates_dirs = crate::args::default_templates_dirs(&state_root);
    }
    let (builder, protected) = util::backend_builder(&g)?;
    c.backend = Some(
        util::backend_name(backend)
            .parse()
            .map_err(anyhow::Error::msg)?,
    );

    // Signals first, so one during start is not lost.
    let mut interrupt = Interrupt::install();
    let server = Server::start(c, builder, protected)
        .await
        .context("cannot start the server")?;
    eprintln!(
        "sandcrab server: {} backend, listening on {}",
        server.sandcrab().backend_kind(),
        server.socket_path().display()
    );
    let lifeline = a.lifeline_stdin;
    let stop = async move {
        if lifeline {
            tokio::select! {
                () = interrupt.wait() => {}
                () = sandcrab_server::stdin_lifeline() => {}
            }
        } else {
            interrupt.wait().await;
        }
    };
    server.run(stop).await.context("stopping the server")?;
    eprintln!("sandcrab server: stopped");
    Ok(0)
}
