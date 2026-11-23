//! Shared CLI plumbing: paths, backend construction, output, termination signals.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use sandcrab_sdk::{Sandcrab, TemplateClient, WorkspaceClient};
use serde::Serialize;
use tokio::sync::watch;

use crate::Global;
use crate::args::{self, BackendArg};

/// Deprecated, no-op: the docker backend used to need this opt-in. Still recognized (with a
/// warning) for one release so existing scripts keep working.
pub const DEPRECATED_INSECURE_ENV: &str = "SANDCRAB_ALLOW_INSECURE";

/// Printed when `--allow-insecure` or `SANDCRAB_ALLOW_INSECURE` is used.
pub const DEPRECATED_INSECURE_WARNING: &str = "warning: --allow-insecure / \
    SANDCRAB_ALLOW_INSECURE is deprecated and has no effect: the docker backend is officially \
    supported and needs no opt-in";

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

/// Warn (once, on stderr) if the deprecated docker opt-in was passed. It changes nothing.
pub fn warn_deprecated_opt_in(g: &Global) {
    let env = std::env::var(DEPRECATED_INSECURE_ENV).ok();
    if args::deprecated_opt_in_used(g.allow_insecure, env.as_deref()) {
        eprintln!("{DEPRECATED_INSECURE_WARNING}");
    }
}

pub fn workspaces(g: &Global) -> anyhow::Result<WorkspaceClient> {
    Ok(WorkspaceClient::open(state_root(g)?)?)
}

pub fn templates(g: &Global) -> anyhow::Result<TemplateClient> {
    Ok(TemplateClient::load(&templates_dirs(g)?)?)
}

/// `--crabd`, else `$SANDCRAB_CRABD`, else `target/<arch>-unknown-linux-musl/release/crabd`.
pub fn crabd_path(g: &Global) -> PathBuf {
    g.crabd.clone().unwrap_or_else(|| {
        std::env::var_os("SANDCRAB_CRABD")
            .filter(|v| !v.is_empty())
            .map_or_else(
                || {
                    PathBuf::from(format!(
                        "target/{}-unknown-linux-musl/release/crabd",
                        std::env::consts::ARCH
                    ))
                },
                PathBuf::from,
            )
    })
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
            builder = builder.docker(sandcrab_sdk::DockerConfig {
                crabd_path: crabd_path(g),
                allow_unpinned_images: g.allow_unpinned,
                ..sandcrab_sdk::DockerConfig::default()
            });
        }
        #[cfg(all(feature = "firecracker", target_os = "linux"))]
        BackendArg::Firecracker => {
            // Hardware isolated. Preflight first so a missing /dev/kvm
            // or artifact is a precise, actionable error (never a fallback).
            let config = crate::firecracker::config(g)?;
            crate::firecracker::preflight(&config)?;
            builder = builder.firecracker(config);
        }
    }
    builder.build().context("cannot initialise the backend")
}

/// One line per spawn when the backend cannot enforce the template's `disk_mb`.
pub fn warn_unenforced_limits(sc: &Sandcrab) {
    if !enforces_disk_limit(sc) {
        eprintln!(
            "warning: backend `{}`: template disk_mb is not enforced",
            sc.backend_kind()
        );
    }
}

fn enforces_disk_limit(sc: &Sandcrab) -> bool {
    #[cfg(feature = "docker")]
    if let Some(d) = sc.docker_backend() {
        return d.enforces_disk_limit();
    }
    let _ = sc;
    true
}

pub fn print_json<T: Serialize + ?Sized>(v: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

pub fn ms(d: Option<Duration>) -> String {
    d.map_or_else(|| "?".into(), |d| d.as_millis().to_string())
}

/// Signals that tear the sandbox down before exiting (exit code `128 + signal`).
#[cfg(unix)]
pub const TERMINATING_SIGNALS: [i32; 4] = [
    2,  // SIGINT
    15, // SIGTERM
    1,  // SIGHUP
    3,  // SIGQUIT
];

/// Termination latch: once installed, SIGINT / SIGTERM / SIGHUP / SIGQUIT no longer kill
/// the process; the first one received is recorded so the command can tear its sandbox
/// down first and then exit with `128 + signal`.
#[derive(Clone)]
pub struct Interrupt(watch::Receiver<Option<i32>>);

impl Interrupt {
    pub fn install() -> Self {
        let (tx, rx) = watch::channel(None);
        #[cfg(unix)]
        for sig in TERMINATING_SIGNALS {
            use tokio::signal::unix::{SignalKind, signal};
            let tx = tx.clone();
            match signal(SignalKind::from_raw(sig)) {
                Ok(mut stream) => {
                    tokio::spawn(async move {
                        while stream.recv().await.is_some() {
                            // Keep the first signal: it decides the exit code.
                            tx.send_if_modified(|v| v.is_none() && v.replace(sig).is_none());
                        }
                    });
                }
                Err(e) => tracing::warn!(signal = sig, error = %e, "cannot install signal handler"),
            }
        }
        #[cfg(not(unix))]
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                tx.send_if_modified(|v| v.is_none() && v.replace(2).is_none());
            }
        });
        Self(rx)
    }

    pub fn is_set(&self) -> bool {
        self.0.borrow().is_some()
    }

    /// The first terminating signal received, if any.
    pub fn signal(&self) -> Option<i32> {
        *self.0.borrow()
    }

    /// Exit code for an interrupted command: `128 + signal`.
    pub fn exit_code(&self) -> u8 {
        exit_code_for(self.signal().unwrap_or(2))
    }

    /// Resolves once a terminating signal has been received.
    pub async fn wait(&mut self) {
        let _ = self.0.wait_for(Option::is_some).await;
    }
}

/// `128 + signal`, saturating at 255.
pub fn exit_code_for(sig: i32) -> u8 {
    u8::try_from((128 + sig).clamp(0, 255)).unwrap_or(255)
}
