//! Orphan reaping (design §7): run dirs whose owning sandcrab process died.
//!
//! For every `<run_dir>/<uuid>/meta.json` with `backend = "firecracker"` whose owner (pid +
//! start time) is gone: SIGKILL the VMM if it is still the same process, preserve rw
//! workspace images of a configured guest in the recovery dir, remove the cgroup leaf and
//! the run dir. Tap locks are `flock`s and died with the owner. Directories without a
//! parseable `meta.json` are only removed once older than [`STALE_AFTER`] (a concurrent
//! spawn writes it right after `mkdir`). Nothing outside `run_dir` is ever deleted.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sandcrab_core::Result;
use uuid::Uuid;

use crate::FirecrackerBackend;
use crate::rundir::{self, CgroupLeaf, META_BACKEND, RunMeta};
use crate::workspace::io_at;

/// Age after which a run dir without metadata is considered abandoned.
pub const STALE_AFTER: Duration = Duration::from_secs(60);

/// What [`FirecrackerBackend::reap_orphans`] cleaned up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReapReport {
    /// Run dirs removed.
    pub run_dirs: Vec<PathBuf>,
    /// VMM pids killed.
    pub killed: Vec<u32>,
    /// Workspace images moved to the recovery dir.
    pub recovered: Vec<PathBuf>,
}

impl ReapReport {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.run_dirs.is_empty() && self.killed.is_empty() && self.recovered.is_empty()
    }
}

/// Reap orphans under `run_dir` (see the module docs).
pub fn reap_run_dir(run_dir: &Path, recovery_dir: &Path) -> Result<ReapReport> {
    let mut report = ReapReport::default();
    let entries = match fs::read_dir(run_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(io_at(run_dir, e)),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|s| Uuid::parse_str(s).ok()) else {
            continue; // `net/` and anything not ours
        };
        let dir = entry.path();
        let Ok(md) = fs::symlink_metadata(&dir) else {
            continue;
        };
        if !md.is_dir() {
            continue;
        }
        let meta = match RunMeta::load(&dir) {
            Ok(m) if m.backend == META_BACKEND && m.sandbox_id.0 == id => m,
            Ok(_) => continue, // someone else's directory
            Err(_) => {
                let age = md.modified().ok().and_then(|m| m.elapsed().ok());
                if age.is_some_and(|a| a > STALE_AFTER) && fs::remove_dir_all(&dir).is_ok() {
                    report.run_dirs.push(dir);
                }
                continue;
            }
        };
        if meta.owner.is_alive() {
            continue;
        }
        if let Some(vmm) = meta.vmm
            && vmm.kill(true)
        {
            report.killed.push(vmm.pid);
            // Give the kernel a moment to tear the VMM down before touching its files.
            for _ in 0..50 {
                if !vmm.is_alive() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if meta.guest_boot_ms.is_some() {
            report.recovered.extend(rundir::preserve_rw_images(
                recovery_dir,
                meta.sandbox_id,
                &meta.rw_drives,
            ));
        }
        if let Some(cg) = &meta.cgroup {
            CgroupLeaf { path: cg.clone() }.remove();
        }
        match fs::remove_dir_all(&dir) {
            Ok(()) => report.run_dirs.push(dir),
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "cannot remove orphan run dir")
            }
        }
    }
    Ok(report)
}

impl FirecrackerBackend {
    /// Reap sandboxes left behind by dead sandcrab processes (runs automatically in
    /// [`FirecrackerBackend::new`]). Sandboxes owned by live processes, including this one,
    /// are never touched.
    pub fn reap_orphans(&self) -> Result<ReapReport> {
        let c = self.config();
        reap_run_dir(&c.run_dir, &c.recovery_dir)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;

    use sandcrab_core::{SandboxId, WorkspaceBacking};

    use super::*;
    use crate::proc::ProcId;
    use crate::rundir::RwDriveMeta;

    #[test]
    fn reaps_dead_owner_keeps_live_and_foreign() {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("run");
        let rec = tmp.path().join("rec");

        // Live owner (this process): untouched.
        let (live, _) = rundir::create(&run, SandboxId::new()).unwrap();

        // Dead owner with a still-running "VMM" and a configured guest's rw image.
        let mut vmm = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let id = SandboxId::new();
        let (dead, mut meta) = rundir::create(&run, id).unwrap();
        meta.owner = ProcId {
            pid: std::process::id(),
            starttime: Some(u64::MAX),
        };
        meta.vmm = ProcId::of(vmm.id());
        meta.guest_boot_ms = Some(100);
        let img = dead.join("ws-0.ext4");
        fs::write(&img, "data").unwrap();
        meta.rw_drives.push(RwDriveMeta {
            drive_id: "ws0".into(),
            image: img,
            workspace_id: "w".into(),
            workspace_name: "proj".into(),
            backing: WorkspaceBacking::HostDir("/p".into()),
            host_path: "/p".into(),
            guest_path: "/workspace".into(),
        });
        meta.save(&dead).unwrap();

        // Foreign / non-uuid entries: untouched.
        fs::create_dir_all(run.join("net")).unwrap();
        let foreign = run.join(Uuid::new_v4().to_string());
        fs::create_dir(&foreign).unwrap(); // fresh, no meta: not stale yet

        let r = reap_run_dir(&run, &rec).unwrap();
        assert_eq!(r.run_dirs, std::slice::from_ref(&dead));
        assert_eq!(r.killed, [vmm.id()]);
        assert_eq!(r.recovered.len(), 1);
        assert_eq!(fs::read_to_string(&r.recovered[0]).unwrap(), "data");
        assert!(r.recovered[0].starts_with(rec.join(id.to_string())));
        assert!(vmm.wait().unwrap().code().is_none(), "killed by a signal");
        assert!(live.is_dir() && foreign.is_dir() && run.join("net").is_dir());

        assert!(reap_run_dir(&run, &rec).unwrap().is_empty());
        assert!(
            reap_run_dir(&tmp.path().join("missing"), &rec)
                .unwrap()
                .is_empty()
        );
    }
}
