//! `sandcrab doctor [--fix [--all [--yes]]]`.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use serde::Serialize;

use crate::Global;
use crate::util::{self, print_json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Info,
    Warn,
    Fail,
}

#[derive(Debug, Serialize)]
struct Check {
    name: String,
    status: Status,
    detail: String,
}

#[derive(Default)]
pub struct Report(Vec<Check>);

/// Orphan reaping requested by `doctor --fix [--all [--yes]]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reap {
    None,
    /// This uid + state dir, owning process gone.
    Owned,
    /// Every managed container on the host; `confirmed` skips the prompt (`--yes`).
    All {
        confirmed: bool,
    },
}

/// Ask before the host-wide sweep. Without a terminal, refuse unless `--yes`.
#[cfg(feature = "docker")]
fn confirm_reap_all() -> anyhow::Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};

    eprintln!(
        "WARNING: `doctor --fix --all` removes EVERY sandcrab docker container on this host,\n\
         including live sandboxes of other sandcrab processes and other users. Running\n\
         commands inside them are killed and unsaved sandbox state is lost."
    );
    if !std::io::stdin().is_terminal() {
        eprintln!("refusing without a terminal to confirm; pass --yes to proceed");
        return Ok(false);
    }
    eprint!("Remove all sandcrab containers? Type `yes` to continue: ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(answer.trim() == "yes")
}

impl Report {
    pub fn add(&mut self, name: impl Into<String>, status: Status, detail: impl Into<String>) {
        self.0.push(Check {
            name: name.into(),
            status,
            detail: detail.into(),
        });
    }
}

fn rust(r: &mut Report) {
    match Command::new("rustc").arg("--version").output() {
        Ok(o) if o.status.success() => r.add(
            "rust",
            Status::Ok,
            String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        ),
        _ => r.add(
            "rust",
            Status::Info,
            "rustc not found (only needed to build crabd: scripts/build-crabd.sh)",
        ),
    }
}

/// Builds without the firecracker backend only report whether KVM exists.
#[cfg(not(all(feature = "firecracker", target_os = "linux")))]
fn kvm(r: &mut Report) {
    if std::path::Path::new("/dev/kvm").exists() {
        r.add(
            "firecracker",
            Status::Info,
            "/dev/kvm present; the firecracker backend is not compiled into this build",
        );
    } else {
        r.add(
            "firecracker",
            Status::Info,
            "Firecracker: unavailable (no /dev/kvm)",
        );
    }
}

fn state_dir(g: &Global, r: &mut Report, fix: bool) {
    let root = match util::state_root(g) {
        Ok(p) => p,
        Err(e) => return r.add("state_dir", Status::Fail, e.to_string()),
    };
    let meta = match std::fs::metadata(&root) {
        Ok(m) => m,
        Err(_) => {
            return r.add(
                "state_dir",
                Status::Info,
                format!(
                    "{} does not exist yet (created 0700 on first use)",
                    root.display()
                ),
            );
        }
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 == 0 {
        r.add(
            "state_dir",
            Status::Ok,
            format!("{} (mode {mode:o})", root.display()),
        );
    } else if fix {
        match std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)) {
            Ok(()) => r.add(
                "state_dir",
                Status::Ok,
                format!("{}: mode {mode:o} → 700 (fixed)", root.display()),
            ),
            Err(e) => r.add(
                "state_dir",
                Status::Fail,
                format!("{}: chmod 700 failed: {e}", root.display()),
            ),
        }
    } else {
        r.add(
            "state_dir",
            Status::Warn,
            format!(
                "{} is mode {mode:o} (group/world accessible); `doctor --fix` sets 700",
                root.display()
            ),
        );
    }
}

/// Which host ↔ crabd transport Docker sandboxes use, and why.
#[cfg(feature = "docker")]
fn transport_note(b: &sandcrab_sdk::docker::DockerBackend) -> String {
    use sandcrab_sdk::docker::{DockerTransport, TRANSPORT_ENV};
    let source = if std::env::var(TRANSPORT_ENV).is_ok_and(|v| !v.trim().is_empty()) {
        format!("from ${TRANSPORT_ENV}")
    } else {
        "host default".to_owned()
    };
    match b.transport() {
        DockerTransport::Unix => {
            format!("unix ({source}): Unix socket in the per-sandbox run dir, token-authenticated")
        }
        DockerTransport::Tcp => format!(
            "tcp ({source}): relay container on 127.0.0.1 (ephemeral port), \
             token-authenticated; sandbox network policy unchanged"
        ),
    }
}

#[cfg(feature = "docker")]
async fn docker(g: &Global, r: &mut Report, reap: Reap) {
    use sandcrab_sdk::core::{ImageArch, PinOrigin, PinStore, effective_pin, verify_image_pin};
    use sandcrab_sdk::docker::{
        DockerBackend, DockerConfig, DockerImageVerifier, check_static_crabd,
    };
    use sandcrab_sdk::{PinStatus, ReapScope, SandboxError};

    let crabd = util::crabd_path(g);
    let crabd_ok = match check_static_crabd(&crabd) {
        Ok(p) => {
            r.add(
                "crabd",
                Status::Ok,
                format!("static ELF at {}", p.display()),
            );
            true
        }
        Err(e) => {
            r.add("crabd", Status::Fail, e.to_string());
            false
        }
    };

    // Daemon: full backend when crabd is usable (doctor never spawns).
    let backend = if crabd_ok {
        let state = util::state_root(g).map(|s| s.join("run")).ok();
        let mut config = DockerConfig {
            crabd_path: crabd,
            ..DockerConfig::default()
        };
        if let Some(s) = state {
            config.state_dir = s;
        }
        match DockerBackend::new(config) {
            Ok(b) => match b.ping().await {
                Ok(v) => {
                    r.add(
                        "docker",
                        Status::Ok,
                        format!("daemon reachable (Docker {v})"),
                    );
                    r.add("docker transport", Status::Info, transport_note(&b));
                    Some(b)
                }
                Err(e) => {
                    r.add("docker", Status::Fail, e.to_string());
                    None
                }
            },
            Err(e) => {
                r.add("docker", Status::Fail, e.to_string());
                None
            }
        }
    } else {
        r.add(
            "docker",
            Status::Info,
            "daemon check skipped (needs a valid crabd)",
        );
        None
    };

    // Template images + pins.
    let templates = match util::templates(g) {
        Ok(t) => t,
        Err(e) => return r.add("templates", Status::Fail, e.to_string()),
    };
    let all = templates.list();
    if all.is_empty() {
        r.add(
            "templates",
            Status::Warn,
            "no templates found (pass --templates-dir or run from the repo root)",
        );
    }
    let verifier = match &backend {
        Some(b) => Some(b.image_verifier()),
        None => DockerImageVerifier::connect(None).ok(),
    };
    let store = match PinStore::open_default() {
        Ok(s) => {
            r.add(
                "template-pins",
                Status::Info,
                format!(
                    "{} (host-local image pins: local integrity, i.e. unchanged since this host \
                     built or adopted them; not provenance)",
                    s.root().display()
                ),
            );
            Some(s)
        }
        Err(e) => {
            r.add("template-pins", Status::Warn, e.to_string());
            None
        }
    };
    for t in &all {
        let name = format!("template:{}", t.name());
        let Some(v) = &verifier else {
            r.add(name, Status::Info, "skipped (no Docker connection)");
            continue;
        };
        let pin = match effective_pin(store.as_ref(), &t.manifest, ImageArch::host()) {
            Ok(p) => p,
            Err(e) => {
                r.add(name, Status::Fail, e.to_string());
                continue;
            }
        };
        let legacy = pin
            .as_ref()
            .is_some_and(|p| p.origin == PinOrigin::LegacyManifest);
        match verify_image_pin(v, t.image(), pin.as_ref().map(|p| p.digest.as_str())).await {
            Ok(PinStatus::Verified { .. }) if legacy => r.add(
                name,
                Status::Warn,
                format!(
                    "{} present, matches the deprecated in-manifest pin; record a host-local \
                     pin with `sandcrab template pin {}`",
                    t.image(),
                    t.name()
                ),
            ),
            Ok(PinStatus::Verified { .. }) => {
                r.add(
                    name,
                    Status::Ok,
                    format!(
                        "{} present, matches this host's pin (local integrity)",
                        t.image()
                    ),
                );
            }
            Ok(PinStatus::Unpinned { digest }) => r.add(
                name,
                Status::Warn,
                if t.manifest.template.external {
                    format!(
                        "{} present ({digest}) but not pinned on this host: spawns are \
                         refused; `sandcrab template pin {}` adopts it",
                        t.image(),
                        t.name()
                    )
                } else {
                    format!(
                        "{} present ({digest}) but not pinned on this host: spawns are refused; \
                         `sandcrab template build {n}` rebuilds and pins it, \
                         `sandcrab template pin {n}` adopts the existing image",
                        t.image(),
                        n = t.name()
                    )
                },
            ),
            Err(SandboxError::DigestMismatch {
                expected, actual, ..
            }) => r.add(
                name,
                Status::Fail,
                format!(
                    "{}: this host's pin {expected} != local image {actual}; {}",
                    t.image(),
                    sandcrab_sdk::core::pins::mismatch_hint(t.name())
                ),
            ),
            Err(SandboxError::NotFound { .. }) => r.add(
                name,
                Status::Fail,
                format!(
                    "{} not present; {}",
                    t.image(),
                    sandcrab_sdk::core::pins::obtain_hint(&t.manifest)
                ),
            ),
            Err(e) => r.add(name, Status::Fail, e.to_string()),
        }
    }

    let scope = match reap {
        Reap::None => return,
        Reap::Owned => ReapScope::Owned,
        Reap::All { confirmed } => {
            if !(confirmed || confirm_reap_all().unwrap_or(false)) {
                return r.add(
                    "orphans",
                    Status::Warn,
                    "host-wide reaping not confirmed; nothing removed",
                );
            }
            ReapScope::All
        }
    };
    match &backend {
        Some(b) => match b.reap_orphans(&Default::default(), scope).await {
            Ok(rep) => r.add(
                "orphans",
                Status::Ok,
                format!(
                    "reaped {} sandbox(es), {} stray socket volume(s), {} socket dir(s){}",
                    rep.containers.len(),
                    rep.volumes.len(),
                    rep.socket_dirs.len(),
                    if rep.kept > 0 {
                        format!(
                            "; kept {} not provably orphaned (live owner, other user / \
                                 state dir, or pre-owner-label; `--fix --all` removes them)",
                            rep.kept
                        )
                    } else {
                        String::new()
                    }
                ),
            ),
            Err(e) => r.add("orphans", Status::Fail, e.to_string()),
        },
        None => r.add(
            "orphans",
            Status::Fail,
            "cannot reap: Docker backend unavailable",
        ),
    }
}

pub async fn doctor(g: &Global, fix: bool, reap: Reap) -> anyhow::Result<u8> {
    let mut r = Report::default();
    rust(&mut r);
    #[cfg(feature = "docker")]
    docker(g, &mut r, reap).await;
    #[cfg(not(feature = "docker"))]
    let _ = reap;
    #[cfg(not(all(feature = "firecracker", target_os = "linux")))]
    kvm(&mut r);
    #[cfg(all(feature = "firecracker", target_os = "linux"))]
    crate::firecracker::doctor(g, &mut r, fix);
    #[cfg(all(
        feature = "libkrun",
        any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
    ))]
    crate::libkrun::doctor(g, &mut r);
    #[cfg(not(all(
        feature = "libkrun",
        any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))
    )))]
    r.add(
        "libkrun",
        Status::Info,
        "libkrun: not compiled into this build (macOS aarch64 and Linux only)",
    );
    state_dir(g, &mut r, fix);

    let ok = r.0.iter().all(|c| c.status != Status::Fail);
    if g.json {
        print_json(&serde_json::json!({ "ok": ok, "checks": r.0 }))?;
    } else {
        for c in &r.0 {
            let tag = match c.status {
                Status::Ok => " ok ",
                Status::Info => "info",
                Status::Warn => "warn",
                Status::Fail => "FAIL",
            };
            println!("[{tag}] {:<20} {}", c.name, c.detail);
        }
    }
    Ok(u8::from(!ok))
}
