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

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
        self.write("cgroup.procs", &pid.to_string())
    }

    fn write(&self, file: &str, value: &str) -> Result<()> {
        let p = self.path.join(file);
        fs::write(&p, value).map_err(|e| io_at(&p, e))
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

#[cfg(test)]
mod tests {
    use super::*;

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
