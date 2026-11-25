//! Per-sandbox run directory `<run_dir>/<sandbox id>/` (0700) and its `meta.json`
//! (design §7), plus the optional cgroup v2 leaf.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sandcrab_core::{Result, RootIdentity, SandboxError, SandboxId, WorkspaceBacking};
use serde::{Deserialize, Serialize};

use crate::proc::ProcId;
use crate::workspace::io_at;

pub const API_SOCK: &str = "api.sock";
pub const VSOCK_SOCK: &str = "v.sock";
pub const LOG_FILE: &str = "firecracker.log";
pub const META_FILE: &str = "meta.json";
pub const SCRATCH_FILE: &str = "scratch.ext4";
/// `backend` value in `meta.json`.
pub const META_BACKEND: &str = "firecracker";

/// A writable workspace drive that must be synced back (or recovered).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RwDriveMeta {
    pub drive_id: String,
    pub image: PathBuf,
    pub workspace_id: String,
    pub workspace_name: String,
    pub backing: WorkspaceBacking,
    pub host_path: PathBuf,
    pub guest_path: PathBuf,
    /// `(dev, ino)` of `host_path` at lease time; re-checked before sync-back.
    #[serde(default)]
    pub root_identity: Option<RootIdentity>,
}

/// `meta.json`: what an orphan reaper needs to clean up after a dead owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMeta {
    pub backend: String,
    pub sandbox_id: SandboxId,
    /// The sandcrab process that owns this sandbox.
    pub owner: ProcId,
    /// The Firecracker process, once launched.
    pub vmm: Option<ProcId>,
    pub tap: Option<String>,
    pub cgroup: Option<PathBuf>,
    #[serde(default)]
    pub rw_drives: Vec<RwDriveMeta>,
    pub guest_boot_ms: Option<u64>,
    pub created_at: DateTime<Utc>,
}

impl RunMeta {
    #[must_use]
    pub fn new(id: SandboxId) -> Self {
        Self {
            backend: META_BACKEND.into(),
            sandbox_id: id,
            owner: ProcId::current(),
            vmm: None,
            tap: None,
            cgroup: None,
            rw_drives: Vec::new(),
            guest_boot_ms: None,
            created_at: Utc::now(),
        }
    }

    pub fn load(dir: &Path) -> io::Result<Self> {
        let data = fs::read(dir.join(META_FILE))?;
        serde_json::from_slice(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Atomic write (temp file + rename).
    pub fn save(&self, dir: &Path) -> Result<()> {
        let tmp = dir.join(".meta.json.tmp");
        let path = dir.join(META_FILE);
        let json = serde_json::to_vec_pretty(self).expect("meta serializes");
        let write = || -> io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&json)?;
            fs::rename(&tmp, &path)
        };
        write().map_err(|e| io_at(&path, e))
    }
}

/// Create `<run_dir>/<id>/` (0700, parents 0700) and write the initial `meta.json`.
pub fn create(run_dir: &Path, id: SandboxId) -> Result<(PathBuf, RunMeta)> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(run_dir)
        .map_err(|e| io_at(run_dir, e))?;
    let dir = run_dir.join(id.to_string());
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| io_at(&dir, e))?;
    let meta = RunMeta::new(id);
    if let Err(e) = meta.save(&dir) {
        let _ = fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok((dir, meta))
}

/// Move each rw drive image into `<recovery_dir>/<sandbox id>/` with an `info.json`, so a
/// failed sync-back never loses the guest's writes. Returns the moved paths.
pub fn preserve_rw_images(
    recovery_dir: &Path,
    id: SandboxId,
    drives: &[RwDriveMeta],
) -> Vec<PathBuf> {
    let mut moved = Vec::new();
    let dest_dir = recovery_dir.join(id.to_string());
    for d in drives {
        if !d.image.is_file() {
            continue;
        }
        if let Err(e) = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dest_dir)
        {
            tracing::warn!(dir = %dest_dir.display(), error = %e, "cannot create recovery dir");
            return moved;
        }
        let Some(name) = d.image.file_name() else {
            continue;
        };
        let dest = dest_dir.join(name);
        match fs::rename(&d.image, &dest) {
            Ok(()) => {
                let mut info = d.clone();
                info.image = dest.clone();
                let _ = fs::write(
                    dest.with_extension("json"),
                    serde_json::to_vec_pretty(&info).expect("serializes"),
                );
                tracing::warn!(
                    sandbox = %id,
                    workspace = %d.workspace_name,
                    image = %dest.display(),
                    "workspace sync-back incomplete; guest image preserved for recovery"
                );
                moved.push(dest);
            }
            Err(e) => tracing::warn!(
                image = %d.image.display(),
                error = %e,
                "cannot move workspace image to the recovery dir"
            ),
        }
    }
    moved
}

/// A cgroup v2 leaf holding one VMM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupLeaf {
    pub path: PathBuf,
}

impl CgroupLeaf {
    /// `mkdir <parent>/sandcrab-<id>` and set `memory.max`, `memory.swap.max = 0` and
    /// `pids.max`. Without the swap cap, memory above `memory.max` would be swapped out
    /// instead of capped (the effective limit would be RAM + swap). A kernel without swap
    /// accounting has no `memory.swap.max`, and then nothing can be swapped past the cap
    /// either, so a missing file is skipped.
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
            .and_then(|()| leaf.write_if_present("memory.swap.max", "0"))
            .and_then(|()| leaf.write("pids.max", &pids_max.to_string()));
        if let Err(e) = res {
            leaf.remove();
            return Err(e);
        }
        Ok(leaf)
    }

    pub fn add(&self, pid: u32) -> Result<()> {
        self.write("cgroup.procs", &pid.to_string())
    }

    fn write(&self, file: &str, value: &str) -> Result<()> {
        let p = self.path.join(file);
        fs::write(&p, value).map_err(|e| io_at(&p, e))
    }

    /// Like [`Self::write`], but an interface file the kernel does not provide (`ENOENT`,
    /// e.g. `memory.swap.max` without swap accounting) is skipped. Opened without `O_CREAT`:
    /// cgroupfs never creates files.
    fn write_if_present(&self, file: &str, value: &str) -> Result<()> {
        use std::io::Write as _;
        let p = self.path.join(file);
        match fs::OpenOptions::new().write(true).truncate(true).open(&p) {
            Ok(mut f) => f.write_all(value.as_bytes()).map_err(|e| io_at(&p, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                tracing::debug!(file = %p.display(), "cgroup interface file absent; skipped");
                Ok(())
            }
            Err(e) => Err(io_at(&p, e)),
        }
    }

    /// Best-effort `rmdir` (the leaf must be empty: call after the VMM was reaped).
    pub fn remove(&self) {
        if let Err(e) = fs::remove_dir(&self.path)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(cgroup = %self.path.display(), error = %e, "cannot remove cgroup leaf");
        }
    }
}

/// Last `max` bytes of the VMM log (for error messages).
#[must_use]
pub fn log_tail(dir: &Path, max: usize) -> String {
    let Ok(data) = fs::read(dir.join(LOG_FILE)) else {
        return String::new();
    };
    let start = data.len().saturating_sub(max);
    String::from_utf8_lossy(&data[start..]).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_meta_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        let run = tmp.path().join("run/fc");
        let (dir, mut meta) = create(&run, id).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        meta.tap = Some("sctap1".into());
        meta.save(&dir).unwrap();
        assert_eq!(RunMeta::load(&dir).unwrap(), meta);
        assert!(create(&run, id).is_err(), "never reuses a run dir");
    }

    #[test]
    fn preserves_images() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        let img = tmp.path().join("ws-0.ext4");
        fs::write(&img, "img").unwrap();
        let d = RwDriveMeta {
            drive_id: "ws0".into(),
            image: img.clone(),
            workspace_id: "w".into(),
            workspace_name: "proj".into(),
            backing: WorkspaceBacking::HostDir("/p".into()),
            host_path: "/p".into(),
            guest_path: "/workspace".into(),
            root_identity: None,
        };
        let missing = RwDriveMeta {
            image: tmp.path().join("ws-1.ext4"),
            ..d.clone()
        };
        let moved = preserve_rw_images(&tmp.path().join("rec"), id, &[d, missing]);
        assert_eq!(moved.len(), 1);
        assert!(!img.exists());
        assert_eq!(fs::read_to_string(&moved[0]).unwrap(), "img");
        assert!(moved[0].with_extension("json").is_file());
    }

    #[test]
    fn swap_is_capped_when_the_kernel_accounts_it() {
        let tmp = tempfile::tempdir().unwrap();
        // No swap accounting (fake fs: no `memory.swap.max`): skipped, not created.
        let leaf = CgroupLeaf::create(tmp.path(), SandboxId::new(), 1 << 30, 64).unwrap();
        assert!(!leaf.path.join("memory.swap.max").exists());
        // With the interface file present it is set to 0.
        fs::write(leaf.path.join("memory.swap.max"), "max").unwrap();
        leaf.write_if_present("memory.swap.max", "0").unwrap();
        assert_eq!(
            fs::read_to_string(leaf.path.join("memory.swap.max")).unwrap(),
            "0"
        );
    }

    #[test]
    fn cgroup_leaf_on_fake_fs() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        let leaf = CgroupLeaf::create(tmp.path(), id, 640 << 20, 64).unwrap();
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
    }
}
