//! Shared CLI plumbing: paths, backend construction, output, Ctrl-C.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use sandcrab_sdk::{Sandcrab, TemplateClient, WorkspaceClient};
use serde::Serialize;
use tokio::sync::watch;

use crate::Global;
use crate::args::{self, BackendArg};

pub const INSECURE_ENV: &str = "SANDCRAB_ALLOW_INSECURE";

pub fn state_root(g: &Global) -> anyhow::Result<PathBuf> {
    match &g.state_dir {
        Some(d) => Ok(d.clone()),
        None => Ok(sandcrab_sdk::core::default_state_root()?),
    }
}

pub fn templates_dirs(g: &Global) -> anyhow::Result<Vec<PathBuf>> {
    if g.templates_dirs.is_empty() {
        Ok(args::default_templates_dirs(&state_root(g)?))
    } else {
        Ok(g.templates_dirs.clone())
    }
}

pub fn allow_insecure(g: &Global) -> bool {
    g.allow_insecure || std::env::var(INSECURE_ENV).is_ok_and(|v| args::truthy(&v))
}

pub fn workspaces(g: &Global) -> anyhow::Result<WorkspaceClient> {
    Ok(WorkspaceClient::open(state_root(g)?)?)
}

pub fn templates(g: &Global) -> anyhow::Result<TemplateClient> {
    Ok(TemplateClient::load(&templates_dirs(g)?)?)
}

#[cfg(feature = "docker")]
pub fn crabd_path(g: &Global) -> PathBuf {
    g.crabd
        .clone()
        .unwrap_or_else(sandcrab_sdk::docker::default_crabd_path)
}

/// Build a [`Sandcrab`] on the explicitly selected backend.
pub fn sandcrab(g: &Global) -> anyhow::Result<Sandcrab> {
    let Some(backend) = g.backend else {
        bail!(
            "no backend selected: pass --backend <{}> (or set SANDCRAB_BACKEND); \
             sandcrab never picks one for you",
            args::compiled_backends()
        );
    };
    let mut builder = Sandcrab::builder().state_dir(state_root(g)?);
    for d in templates_dirs(g)? {
        builder = builder.templates_dir(d);
    }
    match backend {
        #[cfg(feature = "docker")]
        BackendArg::Docker => {
            if !allow_insecure(g) {
                bail!(
                    "the docker backend shares the host kernel and is for local dev / CI only; \
                     pass --allow-insecure or set {INSECURE_ENV}=1 to use it"
                );
            }
            builder = builder.docker(sandcrab_sdk::DockerDevConfig {
                crabd_path: crabd_path(g),
                ..sandcrab_sdk::DockerDevConfig::allow_insecure()
            });
        }
    }
    builder.build().context("cannot initialise the backend")
}

/// The one-line warning printed on every insecure (shared-kernel) spawn.
pub fn warn_if_insecure(sc: &Sandcrab) {
    if !sc.backend_capabilities().hardware_isolated {
        eprintln!(
            "warning: backend `{}` shares the host kernel — not a security boundary; local dev / CI only",
            sc.backend_kind()
        );
    }
}

pub fn print_json<T: Serialize + ?Sized>(v: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

pub fn ms(d: Option<Duration>) -> String {
    d.map_or_else(|| "?".into(), |d| d.as_millis().to_string())
}

/// Ctrl-C latch: once installed, SIGINT no longer kills the process; it flips the flag so
/// the command can tear its sandbox down first.
#[derive(Clone)]
pub struct Interrupt(watch::Receiver<bool>);

impl Interrupt {
    pub fn install() -> Self {
        let (tx, rx) = watch::channel(false);
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                if tx.send(true).is_err() {
                    break;
                }
            }
        });
        Self(rx)
    }

    pub fn is_set(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once Ctrl-C has been pressed.
    pub async fn wait(&mut self) {
        let _ = self.0.wait_for(|v| *v).await;
    }
}
