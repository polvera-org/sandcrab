//! Linux cgroup v2 leaf for the helper (`memory.max`, `pids.max`) (design §10).
//!
//! Only used when [`LibkrunConfig::cgroup_parent`](crate::LibkrunConfig::cgroup_parent) names
//! a cgroup v2 directory **delegated** to the sandcrab user (e.g. a systemd user slice with
//! `Delegate=yes`). Each helper gets a `sandcrab-<sandbox id>` leaf with
//! `memory.max = guest RAM + vmm_overhead_mib` and `pids.max`, and is moved into it before it
//! reads its config (so before the VM exists). Without a delegated parent there is **no
//! host-side cap**: the guest RAM bounds the guest, but VMM overhead (virtio-fs server
//! threads, page cache of the device model) is unbounded. That limitation is the same on
//! macOS, which has no cgroups at all; `doctor` surfaces it.
//!
//! Copied from the Firecracker backend's `CgroupLeaf` (design §10: copy first, deduplicate
//! later).
//!
//! Moving a pid between cgroups needs write access to `cgroup.procs` of the **common
//! ancestor** of the source and destination cgroups. A process in an SSH/login session scope
//! (`user-<uid>.slice/session-N.scope`) therefore cannot move its helper into the systemd user
//! manager's delegated tree (`user@<uid>.service/…`): their common ancestor is root-owned. The
//! owning process must itself run inside the delegated tree, e.g. under
//! `systemd-run --user --scope`. [`check_parent`] verifies all of this up front, so a configured
//! but unusable parent fails at backend construction instead of at the first spawn
//! (`docs/hosting.md`, "libkrun on Linux").

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use sandcrab_core::{Result, SandboxError, SandboxId};

use crate::reap::io_at;

/// A cgroup v2 leaf holding one helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupLeaf {
    pub path: PathBuf,
}

impl CgroupLeaf {
    /// `mkdir <parent>/sandcrab-<id>` and set `memory.max` / `pids.max`.
    pub fn create(parent: &Path, id: SandboxId, memory_max: u64, pids_max: u32) -> Result<Self> {
        let path = parent.join(format!("sandcrab-{id}"));
        fs::create_dir(&path).map_err(|e| {
            SandboxError::InvalidConfig(format!(
                "cannot create cgroup leaf {} (is the cgroup delegated to this user?): {e}",
                path.display()
            ))
        })?;
        let leaf = Self { path };
        let res = leaf
            .write("memory.max", &memory_max.to_string())
            .and_then(|()| leaf.write("pids.max", &pids_max.to_string()));
        if let Err(e) = res {
            leaf.remove();
            return Err(e);
        }
        Ok(leaf)
    }

    /// Move `pid` into the leaf.
    pub fn add(&self, pid: u32) -> Result<()> {
        let p = self.path.join("cgroup.procs");
        fs::write(&p, pid.to_string()).map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                SandboxError::InvalidConfig(format!(
                    "cannot move the libkrun helper into {}: {e}; {MIGRATE_HINT}",
                    self.path.display()
                ))
            } else {
                io_at(&p, e)
            }
        })
    }

    fn write(&self, file: &str, value: &str) -> Result<()> {
        let p = self.path.join(file);
        fs::write(&p, value).map_err(|e| io_at(&p, e))
    }

    /// Drop path (the helper may still be dying): `cgroup.kill` the leaf, then retry the
    /// `rmdir` until it is empty or `timeout` passes. A plain [`Self::remove`] right after a
    /// SIGKILL fails with `EBUSY`, and the run dir that would let the reaper retry is gone by
    /// then, so the leaf would leak.
    pub fn kill_and_remove(&self, timeout: std::time::Duration) {
        let _ = fs::write(self.path.join("cgroup.kill"), "1");
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match fs::remove_dir(&self.path) {
                Ok(()) => return,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return,
                Err(e) if std::time::Instant::now() >= deadline => {
                    tracing::warn!(
                        cgroup = %self.path.display(), error = %e,
                        "cannot remove cgroup leaf"
                    );
                    return;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
    }

    /// Best-effort `rmdir` (the leaf must be empty: call after the helper was reaped).
    pub fn remove(&self) {
        if let Err(e) = fs::remove_dir(&self.path)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(cgroup = %self.path.display(), error = %e, "cannot remove cgroup leaf");
        }
    }
}

/// Mount point of the cgroup v2 hierarchy.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Controllers each helper leaf limits.
const CONTROLLERS: [&str; 2] = ["memory", "pids"];

const MIGRATE_HINT: &str = "the sandcrab process must run inside the delegated cgroup tree \
     (e.g. `systemd-run --user --scope -- <command>`), see docs/hosting.md";

/// Validate a configured `cgroup_parent` before any spawn: a writable cgroup v2 directory with
/// `memory` and `pids` enabled in `cgroup.subtree_control`, which this process may move its
/// helpers into.
pub fn check_parent(parent: &Path) -> Result<()> {
    let self_cg = Path::new("/proc/self/cgroup");
    let own = fs::read_to_string(self_cg).map_err(|e| io_at(self_cg, e))?;
    let own = own
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or_else(|| {
            SandboxError::InvalidConfig("libkrun cgroup_parent: this host has no cgroup v2".into())
        })?;
    check_parent_at(Path::new(CGROUP_ROOT), parent, own).map_err(|m| {
        SandboxError::InvalidConfig(format!("libkrun cgroup_parent {}: {m}", parent.display()))
    })
}

/// [`check_parent`] against a given hierarchy root and own cgroup (`/proc/self/cgroup` path).
pub(crate) fn check_parent_at(
    root: &Path,
    parent: &Path,
    own: &str,
) -> std::result::Result<(), String> {
    let rel = parent
        .strip_prefix(root)
        .map_err(|_| format!("not under {}", root.display()))?;
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err("must be a normalized absolute path".into());
    }
    let sub = parent.join("cgroup.subtree_control");
    let enabled = fs::read_to_string(&sub)
        .map_err(|e| format!("{e} reading {} (not a cgroup v2 directory?)", sub.display()))?;
    let missing: Vec<_> = CONTROLLERS
        .iter()
        .filter(|c| !enabled.split_whitespace().any(|e| e == **c))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "controllers {missing:?} not enabled for its children (`echo '+memory +pids' > {}`)",
            sub.display()
        ));
    }
    if !writable(parent) {
        return Err("not writable by this user (is it delegated, e.g. `Delegate=yes`?)".into());
    }
    // Migration needs write access to cgroup.procs of the common ancestor of both cgroups.
    let own = Path::new(own.trim());
    let own = own.strip_prefix("/").unwrap_or(own);
    let common: PathBuf = rel
        .components()
        .zip(own.components())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a)
        .collect();
    let procs = root.join(&common).join("cgroup.procs");
    if !writable(&procs) {
        return Err(format!(
            "this process (cgroup /{}) cannot move helpers there: {} is not writable; \
             {MIGRATE_HINT}",
            own.display(),
            procs.display()
        ));
    }
    Ok(())
}

fn writable(p: &Path) -> bool {
    rustix::fs::access(p, rustix::fs::Access::WRITE_OK).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_checks_on_fake_fs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let user = root.join("user.slice/user-1000.slice/user@1000.service");
        let parent = user.join("app.slice/sandcrab-vms.service");
        fs::create_dir_all(&parent).unwrap();
        for d in [&user.join("app.slice"), &parent] {
            fs::write(d.join("cgroup.procs"), "").unwrap();
        }
        let sub = parent.join("cgroup.subtree_control");
        let inside = "/user.slice/user-1000.slice/user@1000.service/app.slice/run-x.scope";
        let session = "/user.slice/user-1000.slice/session-7.scope";

        fs::write(&sub, "pids\n").unwrap();
        let e = check_parent_at(root, &parent, inside).unwrap_err();
        assert!(e.contains("memory") && e.contains("+memory +pids"), "{e}");
        fs::write(&sub, "memory pids\n").unwrap();
        check_parent_at(root, &parent, inside).unwrap();

        // From a session scope the common ancestor is user-1000.slice: root-owned on a real
        // host; here it has no cgroup.procs, so it is not writable either.
        let e = check_parent_at(root, &parent, session).unwrap_err();
        assert!(e.contains("systemd-run --user --scope"), "{e}");
        assert!(e.contains("user-1000.slice/cgroup.procs"), "{e}");

        let e = check_parent_at(root, Path::new("/elsewhere"), inside).unwrap_err();
        assert!(e.contains("not under"), "{e}");
        let e = check_parent_at(root, &root.join("missing"), inside).unwrap_err();
        assert!(e.contains("cgroup v2"), "{e}");
    }

    #[test]
    fn leaf_on_fake_fs() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        let leaf = CgroupLeaf::create(tmp.path(), id, 640 << 20, 64).unwrap();
        assert!(leaf.path.ends_with(format!("sandcrab-{id}")));
        assert_eq!(
            fs::read_to_string(leaf.path.join("memory.max")).unwrap(),
            "671088640"
        );
        assert_eq!(
            fs::read_to_string(leaf.path.join("pids.max")).unwrap(),
            "64"
        );
        leaf.add(42).unwrap();
        assert_eq!(
            fs::read_to_string(leaf.path.join("cgroup.procs")).unwrap(),
            "42"
        );
        let err = CgroupLeaf::create(&tmp.path().join("missing"), id, 1, 1).unwrap_err();
        assert!(err.to_string().contains("delegated"), "{err}");
        // A real cgroupfs rmdir works on a populated-file leaf; tempfs does not, so only
        // check that remove() never panics.
        leaf.remove();
    }
}
