//! Orphan reaping: remove Sandcrab-managed containers (and socket dirs) whose sandbox is
//! no longer tracked — e.g. after a crash or a cancelled spawn. Used by `doctor --fix`.
//!
//! Only containers carrying **both** `sandcrab.managed=true` and
//! `sandcrab.backend=docker-dev` are considered; unlabeled containers are never touched.
//!
//! Every container also records its [`Owner`] (host uid, pid, boot id, state dir). The
//! default [`ReapScope::Owned`] only removes containers spawned by **this uid** for **this
//! state dir** whose owning process is gone (dead pid, or a previous boot) — live sandboxes
//! of other processes or users survive. [`ReapScope::All`] is the host-wide sweep.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use bollard::query_parameters::ListContainersOptionsBuilder;
use sandcrab_core::{Result, SandboxId};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::DockerDevBackend;
use crate::backend::{backend_err, teardown};
use crate::spec::{
    BACKEND_LABEL_VALUE, LABEL_BACKEND, LABEL_MANAGED, LABEL_OWNER_BOOT_ID, LABEL_OWNER_PID,
    LABEL_OWNER_UID, LABEL_SANDBOX_ID, LABEL_STATE_DIR,
};

/// Socket dirs younger than this are left alone by [`ReapScope::Owned`]: another process
/// sharing the state dir may be between "create socket dir" and "create container".
const SOCKET_DIR_MIN_AGE: Duration = Duration::from_secs(60);

/// Which untracked containers [`DockerDevBackend::reap_orphans`] may remove.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReapScope {
    /// Only containers whose owner labels say: same uid, same state dir, and the owning
    /// process is dead (or ran in a previous boot). Containers without owner labels
    /// (spawned before they existed) are kept.
    #[default]
    Owned,
    /// Every managed docker-dev container on the daemon — including live sandboxes of
    /// other processes and users. Destructive; ask first.
    All,
}

/// The process that spawned a container, recorded as labels at spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    /// Host uid.
    pub uid: u32,
    /// Host pid.
    pub pid: u32,
    /// `/proc/sys/kernel/random/boot_id` (empty where unavailable).
    pub boot_id: String,
    /// [`state_dir_key`] of the socket state directory.
    pub state_dir: String,
}

impl Owner {
    /// The current process, for sockets under `state_dir`.
    #[must_use]
    pub fn current(state_dir: &Path) -> Self {
        Self {
            uid: rustix::process::getuid().as_raw(),
            pid: std::process::id(),
            boot_id: boot_id(),
            state_dir: state_dir_key(state_dir),
        }
    }

    pub(crate) fn labels(&self) -> [(&'static str, String); 4] {
        [
            (LABEL_OWNER_UID, self.uid.to_string()),
            (LABEL_OWNER_PID, self.pid.to_string()),
            (LABEL_OWNER_BOOT_ID, self.boot_id.clone()),
            (LABEL_STATE_DIR, self.state_dir.clone()),
        ]
    }
}

/// Current boot id, or `""` when the platform has none.
#[must_use]
pub fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// First 16 hex chars of the SHA-256 of the canonical `dir` (as given if it does not exist).
#[must_use]
pub fn state_dir_key(dir: &Path) -> String {
    let canon = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let digest = Sha256::digest(canon.as_os_str().as_encoded_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// `kill(pid, 0)`: alive unless the kernel says "no such process".
fn pid_alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    !matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    )
}

/// Whether an untracked managed container may be reaped under `scope`. Pure: the liveness
/// probe is injected.
pub(crate) fn is_orphan(
    labels: &HashMap<String, String>,
    me: &Owner,
    scope: ReapScope,
    pid_alive: impl Fn(u32) -> bool,
) -> bool {
    if scope == ReapScope::All {
        return true;
    }
    let get = |k: &str| labels.get(k).map(String::as_str);
    let (Some(uid), Some(pid), Some(boot), Some(dir)) = (
        get(LABEL_OWNER_UID).and_then(|v| v.parse::<u32>().ok()),
        get(LABEL_OWNER_PID).and_then(|v| v.parse::<u32>().ok()),
        get(LABEL_OWNER_BOOT_ID),
        get(LABEL_STATE_DIR),
    ) else {
        // Pre-owner-label containers: only `ReapScope::All` may remove them.
        return false;
    };
    if uid != me.uid || dir != me.state_dir {
        return false;
    }
    if boot != me.boot_id {
        // Spawned in a previous boot: that process is gone, whatever owns the pid now.
        return true;
    }
    // Our own pid: the container is untracked by this process (cancelled spawn) → orphan.
    pid == me.pid || !pid_alive(pid)
}

/// A container removed by [`DockerDevBackend::reap_orphans`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapedContainer {
    pub container_id: String,
    pub name: Option<String>,
    /// From the `sandcrab.sandbox_id` label, when it parses.
    pub sandbox_id: Option<SandboxId>,
}

/// Outcome of [`DockerDevBackend::reap_orphans`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReapReport {
    pub containers: Vec<ReapedContainer>,
    /// Stale per-sandbox socket directories removed from `state_dir`.
    pub socket_dirs: Vec<std::path::PathBuf>,
    /// Untracked managed containers left alone because [`ReapScope::Owned`] did not
    /// prove them orphaned (other live process, other user / state dir, no owner labels).
    pub kept: usize,
}

fn is_managed(labels: &HashMap<String, String>) -> bool {
    labels.get(LABEL_MANAGED).map(String::as_str) == Some("true")
        && labels.get(LABEL_BACKEND).map(String::as_str) == Some(BACKEND_LABEL_VALUE)
}

impl DockerDevBackend {
    /// The owner recorded on containers this backend spawns.
    #[must_use]
    pub fn owner(&self) -> Owner {
        Owner::current(&self.config.state_dir)
    }

    /// Remove every managed docker-dev container that is not in `tracked` and that `scope`
    /// allows (see [`ReapScope`]), then every `state_dir/<uuid>` socket directory that is
    /// neither tracked nor backed by a remaining container (with [`ReapScope::Owned`], only
    /// directories older than a minute).
    pub async fn reap_orphans(
        &self,
        tracked: &HashSet<SandboxId>,
        scope: ReapScope,
    ) -> Result<ReapReport> {
        let me = self.owner();
        let filters = HashMap::from([(
            "label",
            vec![
                format!("{LABEL_MANAGED}=true"),
                format!("{LABEL_BACKEND}={BACKEND_LABEL_VALUE}"),
            ],
        )]);
        let containers = self
            .docker
            .list_containers(Some(
                ListContainersOptionsBuilder::new()
                    .all(true)
                    .filters(&filters)
                    .build(),
            ))
            .await
            .map_err(|e| backend_err("list containers", e))?;

        let mut report = ReapReport::default();
        let mut live = HashSet::new();
        for c in containers {
            let labels = c.labels.unwrap_or_default();
            // Belt and braces on top of the daemon-side filter.
            if !is_managed(&labels) {
                continue;
            }
            let Some(container_id) = c.id else { continue };
            let sandbox_id = labels
                .get(LABEL_SANDBOX_ID)
                .and_then(|s| Uuid::parse_str(s).ok())
                .map(SandboxId);
            if let Some(id) = sandbox_id
                && tracked.contains(&id)
            {
                live.insert(id);
                continue;
            }
            if !is_orphan(&labels, &me, scope, pid_alive) {
                report.kept += 1;
                if let Some(id) = sandbox_id {
                    live.insert(id);
                }
                continue;
            }
            let dir = sandbox_id.map(|id| self.socket_dir(id));
            teardown(
                &self.docker,
                &container_id,
                dir.as_deref(),
                Duration::ZERO,
                None,
            )
            .await?;
            report.containers.push(ReapedContainer {
                container_id,
                name: c
                    .names
                    .and_then(|n| n.into_iter().next())
                    .map(|n| n.trim_start_matches('/').to_owned()),
                sandbox_id,
            });
        }

        let entries = match fs::read_dir(&self.config.state_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|n| Uuid::parse_str(n).ok())
                .map(SandboxId)
            else {
                continue;
            };
            if tracked.contains(&id) || live.contains(&id) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_dir() {
                continue;
            }
            if scope == ReapScope::Owned {
                let age = meta
                    .modified()
                    .ok()
                    .and_then(|m| SystemTime::now().duration_since(m).ok());
                if age.is_none_or(|a| a < SOCKET_DIR_MIN_AGE) {
                    continue;
                }
            }
            let path = entry.path();
            if fs::remove_dir_all(&path).is_ok() {
                report.socket_dirs.push(path);
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_label_check() {
        let mut l = HashMap::new();
        assert!(!is_managed(&l));
        l.insert(LABEL_MANAGED.to_owned(), "true".to_owned());
        assert!(!is_managed(&l));
        l.insert(LABEL_BACKEND.to_owned(), "firecracker".to_owned());
        assert!(!is_managed(&l));
        l.insert(LABEL_BACKEND.to_owned(), BACKEND_LABEL_VALUE.to_owned());
        assert!(is_managed(&l));
        l.insert(LABEL_MANAGED.to_owned(), "false".to_owned());
        assert!(!is_managed(&l));
    }

    fn me() -> Owner {
        Owner {
            uid: 1000,
            pid: 500,
            boot_id: "boot-now".into(),
            state_dir: "aaaaaaaaaaaaaaaa".into(),
        }
    }

    fn labels_of(o: &Owner) -> HashMap<String, String> {
        o.labels()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect()
    }

    #[test]
    fn orphan_decision() {
        let me = me();
        let alive = |_: u32| true;
        let dead = |_: u32| false;
        let other = |f: fn(&mut Owner)| {
            let mut o = Owner {
                pid: 700,
                ..me.clone()
            };
            f(&mut o);
            labels_of(&o)
        };
        let owned = ReapScope::Owned;

        // Same uid + state dir: reaped only when the owning process is gone.
        assert!(!is_orphan(&other(|_| {}), &me, owned, alive));
        assert!(is_orphan(&other(|_| {}), &me, owned, dead));
        // Previous boot: the pid may have been reused — still an orphan.
        assert!(is_orphan(
            &other(|o| o.boot_id = "boot-old".into()),
            &me,
            owned,
            alive
        ));
        // Our own pid but untracked: a cancelled spawn of ours.
        assert!(is_orphan(&labels_of(&me), &me, owned, alive));

        // Another user or another state dir: never, even if dead / previous boot.
        assert!(!is_orphan(&other(|o| o.uid = 0), &me, owned, dead));
        assert!(!is_orphan(
            &other(|o| {
                o.uid = 1001;
                o.boot_id = "boot-old".into();
            }),
            &me,
            owned,
            dead
        ));
        assert!(!is_orphan(
            &other(|o| o.state_dir = "bbbbbbbbbbbbbbbb".into()),
            &me,
            owned,
            dead
        ));

        // Missing / malformed owner labels (pre-change containers): kept unless `All`.
        assert!(!is_orphan(&HashMap::new(), &me, owned, dead));
        let mut bad = other(|_| {});
        bad.insert(LABEL_OWNER_PID.to_owned(), "nope".to_owned());
        assert!(!is_orphan(&bad, &me, owned, dead));

        // `All`: everything untracked goes.
        assert!(is_orphan(&HashMap::new(), &me, ReapScope::All, alive));
        assert!(is_orphan(&other(|o| o.uid = 0), &me, ReapScope::All, alive));
    }

    #[test]
    fn pid_probe_and_owner() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        let tmp = tempfile::tempdir().unwrap();
        let o = Owner::current(tmp.path());
        assert_eq!(o.pid, std::process::id());
        assert_eq!(o.state_dir.len(), 16);
        // Canonicalised: a `..` detour names the same dir.
        let detour = tmp.path().join("x/..");
        std::fs::create_dir(tmp.path().join("x")).unwrap();
        assert_eq!(state_dir_key(&detour), o.state_dir);
        assert_ne!(state_dir_key(&tmp.path().join("x")), o.state_dir);
        if cfg!(target_os = "linux") {
            assert!(!o.boot_id.is_empty());
        }
    }
}
