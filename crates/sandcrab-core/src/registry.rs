//! Persistent workspace registry with host-side file operations and mount leases.
//!
//! State layout under the state root (default `~/.sandcrab/`, `$SANDCRAB_HOME` overrides):
//!
//! ```text
//! <root>/registry.json            # all workspaces (atomic write: tmp + fsync + rename)
//! <root>/registry.lock            # flock held for every registry read-modify-write
//! <root>/workspaces/<id>/data/    # ManagedDir contents
//! <root>/workspaces/<id>/...      # per-workspace backend state (ro image cache, staging, recovery)
//! ```
//!
//! Host-side file operations (`put_file` / `read_file` / `delete_file` / `list_files`)
//! treat the workspace contents as hostile (a sandbox may be rewriting them concurrently).
//! They refuse `..` and absolute paths, and walk fd-relative from an opened root handle
//! without following any symlink (see [`crate::fsx`]): a symlink anywhere on the path —
//! even one pointing inside the workspace — is refused with [`SandboxError::PathEscape`].
//! `list_files` reports symlinks and `delete_file` removes the link itself. Mutation is
//! refused while a sandbox holds a read-write lease.
//!
//! Leases (`lease()`) are tracked in-process; the registry file itself is safe to share
//! between processes (CLI + SDK), but two processes leasing the same workspace read-write
//! is not detected in V1.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::fsx;
use crate::workspace::validate_workspace_name;
use crate::{
    MountMode, Result, SafeRelPath, SandboxError, SandboxId, Workspace, WorkspaceBacking,
    WorkspaceId, WorkspaceRef, WorkspaceState,
};

/// Environment variable overriding the default state root.
pub const STATE_ROOT_ENV: &str = "SANDCRAB_HOME";

const REGISTRY_FILE: &str = "registry.json";
const LOCK_FILE: &str = "registry.lock";
const REGISTRY_VERSION: u32 = 1;

/// `$SANDCRAB_HOME`, else `$HOME/.sandcrab`.
pub fn default_state_root() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os(STATE_ROOT_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".sandcrab"))
        .ok_or_else(|| {
            SandboxError::InvalidConfig(format!(
                "cannot locate the state root: set ${STATE_ROOT_ENV} or $HOME"
            ))
        })
}

/// Kind of a directory entry returned by [`WorkspaceRegistry::list_files`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One directory entry, relative to the workspace root. Symlinks are reported, not followed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub kind: FileKind,
    pub size: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct RegistryDb {
    version: u32,
    #[serde(default)]
    workspaces: Vec<Workspace>,
}

impl Default for RegistryDb {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            workspaces: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct LeaseCount {
    rw: Option<SandboxId>,
    ro: u32,
}

#[derive(Debug)]
struct Inner {
    /// Canonical state root.
    root: PathBuf,
    /// Serializes registry operations within this process (the flock covers other processes).
    op: Mutex<()>,
    leases: Mutex<HashMap<WorkspaceId, LeaseCount>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(unix)]
fn create_private_dir_all(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| SandboxError::io_at(path, e))
}

#[cfg(not(unix))]
fn create_private_dir_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|e| SandboxError::io_at(path, e))
}

impl Inner {
    fn with_db<R>(&self, mutate: bool, f: impl FnOnce(&mut RegistryDb) -> Result<R>) -> Result<R> {
        let _guard = lock(&self.op);
        let lock_path = self.root.join(LOCK_FILE);
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| SandboxError::io_at(&lock_path, e))?;
        if mutate {
            lock_file.lock()
        } else {
            lock_file.lock_shared()
        }
        .map_err(|e| SandboxError::io_at(&lock_path, e))?;
        let mut db = self.load()?;
        let out = f(&mut db)?;
        if mutate {
            self.store(&db)?;
        }
        Ok(out)
    }

    fn load(&self) -> Result<RegistryDb> {
        let path = self.root.join(REGISTRY_FILE);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(RegistryDb::default()),
            Err(e) => return Err(SandboxError::io_at(&path, e)),
        };
        let db: RegistryDb = serde_json::from_slice(&bytes).map_err(|e| {
            SandboxError::Workspace(format!("corrupt registry {}: {e}", path.display()))
        })?;
        if db.version > REGISTRY_VERSION {
            return Err(SandboxError::Workspace(format!(
                "registry {} has version {}, this build supports up to {REGISTRY_VERSION}",
                path.display(),
                db.version
            )));
        }
        Ok(db)
    }

    fn store(&self, db: &RegistryDb) -> Result<()> {
        let path = self.root.join(REGISTRY_FILE);
        let tmp = self
            .root
            .join(format!("{REGISTRY_FILE}.tmp.{}", std::process::id()));
        let bytes = serde_json::to_vec_pretty(db)
            .map_err(|e| SandboxError::Workspace(format!("cannot serialize registry: {e}")))?;
        let write = || -> io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            fs::rename(&tmp, &path)?;
            // Persist the rename itself; best effort on filesystems that refuse dir fsync.
            if let Ok(dir) = fs::File::open(&self.root) {
                let _ = dir.sync_all();
            }
            Ok(())
        };
        write().map_err(|e| {
            let _ = fs::remove_file(&tmp);
            SandboxError::io_at(&path, e)
        })
    }

    fn workspace_dir(&self, id: WorkspaceId) -> PathBuf {
        self.root.join("workspaces").join(id.to_string())
    }

    /// Overlay in-memory lease state onto a persisted workspace.
    fn decorate(&self, mut ws: Workspace) -> Workspace {
        if ws.state == WorkspaceState::Idle
            && let Some(l) = lock(&self.leases).get(&ws.id)
        {
            ws.state = match l.rw {
                Some(sandbox) => WorkspaceState::LeasedRw { sandbox },
                None if l.ro > 0 => WorkspaceState::LeasedRo { holders: l.ro },
                None => WorkspaceState::Idle,
            };
        }
        ws
    }

    fn is_leased(&self, id: WorkspaceId) -> bool {
        lock(&self.leases)
            .get(&id)
            .is_some_and(|l| l.rw.is_some() || l.ro > 0)
    }

    /// Refuse host-side mutation while a sandbox can write, or while a session is dirty.
    fn ensure_mutable(&self, ws: &Workspace) -> Result<()> {
        if ws.state == WorkspaceState::DirtySession {
            return Err(SandboxError::WorkspaceBusy(format!(
                "workspace `{}` has a dirty session; recover or clear it first",
                ws.name
            )));
        }
        if let Some(sandbox) = lock(&self.leases).get(&ws.id).and_then(|l| l.rw) {
            return Err(SandboxError::WorkspaceBusy(format!(
                "workspace `{}` is mounted read-write by sandbox {sandbox}",
                ws.name
            )));
        }
        Ok(())
    }

    fn release(&self, id: WorkspaceId, mode: MountMode) {
        let mut leases = lock(&self.leases);
        if let Some(l) = leases.get_mut(&id) {
            match mode {
                MountMode::ReadWrite => l.rw = None,
                MountMode::ReadOnly => l.ro = l.ro.saturating_sub(1),
            }
            if l.rw.is_none() && l.ro == 0 {
                leases.remove(&id);
            }
        }
    }
}

fn find<'a>(db: &'a mut RegistryDb, r: &WorkspaceRef) -> Result<&'a mut Workspace> {
    db.workspaces
        .iter_mut()
        .find(|w| r.matches(w))
        .ok_or_else(|| SandboxError::not_found("workspace", r))
}

fn touch(ws: &mut Workspace) {
    ws.generation += 1;
    ws.updated_at = Utc::now();
}

/// Canonical root directory of a directory-backed workspace.
fn dir_root(ws: &Workspace) -> Result<PathBuf> {
    match &ws.backing {
        WorkspaceBacking::HostDir(p) | WorkspaceBacking::ManagedDir(p) => {
            let canon = fs::canonicalize(p).map_err(|e| {
                SandboxError::Workspace(format!(
                    "backing directory {} of workspace `{}` is unavailable: {e}",
                    p.display(),
                    ws.name
                ))
            })?;
            if !canon.is_dir() {
                return Err(SandboxError::Workspace(format!(
                    "backing path {} of workspace `{}` is not a directory",
                    canon.display(),
                    ws.name
                )));
            }
            Ok(canon)
        }
        WorkspaceBacking::BlockVolume(_) => {
            Err(SandboxError::Unimplemented("block volume host mutation"))
        }
    }
}

/// Persistent registry of workspaces. Cheap to clone (shared state).
#[derive(Debug, Clone)]
pub struct WorkspaceRegistry {
    inner: Arc<Inner>,
}

impl WorkspaceRegistry {
    /// Open (creating if needed, mode 0700) the registry at `state_root`.
    pub fn open(state_root: impl AsRef<Path>) -> Result<Self> {
        let state_root = state_root.as_ref();
        create_private_dir_all(&state_root.join("workspaces"))?;
        let root = fs::canonicalize(state_root).map_err(|e| SandboxError::io_at(state_root, e))?;
        let reg = Self {
            inner: Arc::new(Inner {
                root,
                op: Mutex::new(()),
                leases: Mutex::new(HashMap::new()),
            }),
        };
        // Fail fast on a corrupt or future-version registry.
        reg.inner.with_db(false, |_| Ok(()))?;
        Ok(reg)
    }

    /// Open the registry at [`default_state_root`].
    pub fn open_default() -> Result<Self> {
        Self::open(default_state_root()?)
    }

    /// Canonical state root.
    #[must_use]
    pub fn state_root(&self) -> &Path {
        &self.inner.root
    }

    /// `<root>/workspaces/<id>`: per-workspace state dir (managed data, caches, recovery).
    #[must_use]
    pub fn workspace_dir(&self, id: WorkspaceId) -> PathBuf {
        self.inner.workspace_dir(id)
    }

    /// Create a Sandcrab-managed workspace under `<root>/workspaces/<id>/data`.
    pub fn create(&self, name: &str) -> Result<Workspace> {
        validate_workspace_name(name)?;
        let ws = self.inner.with_db(true, |db| {
            ensure_unique_name(db, name)?;
            let id = WorkspaceId::new();
            let data = self.inner.workspace_dir(id).join("data");
            create_private_dir_all(&data)?;
            let ws = new_workspace(id, name, WorkspaceBacking::ManagedDir(data));
            db.workspaces.push(ws.clone());
            Ok(ws)
        })?;
        Ok(ws)
    }

    /// Register an existing host directory as a workspace. The path is canonicalized; `/`,
    /// the state root, and anything overlapping it are refused, as is a directory that is
    /// already registered.
    pub fn register_host_dir(&self, name: &str, path: impl AsRef<Path>) -> Result<Workspace> {
        validate_workspace_name(name)?;
        let path = path.as_ref();
        let canon = fs::canonicalize(path).map_err(|e| {
            SandboxError::InvalidConfig(format!("cannot register `{}`: {e}", path.display()))
        })?;
        if !canon.is_dir() {
            return Err(SandboxError::InvalidConfig(format!(
                "cannot register `{}`: not a directory",
                canon.display()
            )));
        }
        if canon.parent().is_none() {
            return Err(SandboxError::InvalidConfig(
                "refusing to register the filesystem root `/` as a workspace".into(),
            ));
        }
        let root = &self.inner.root;
        if canon.starts_with(root) || root.starts_with(&canon) {
            return Err(SandboxError::InvalidConfig(format!(
                "refusing to register `{}`: it overlaps the Sandcrab state root `{}`",
                canon.display(),
                root.display()
            )));
        }
        self.inner.with_db(true, |db| {
            ensure_unique_name(db, name)?;
            if let Some(other) = db
                .workspaces
                .iter()
                .find(|w| w.backing == WorkspaceBacking::HostDir(canon.clone()))
            {
                return Err(SandboxError::AlreadyExists {
                    kind: "host dir workspace",
                    id: format!("{} (registered as `{}`)", canon.display(), other.name),
                });
            }
            let ws = new_workspace(WorkspaceId::new(), name, WorkspaceBacking::HostDir(canon));
            db.workspaces.push(ws.clone());
            Ok(ws)
        })
    }

    /// Register an existing ext4 image as a block-volume workspace (reserved, see design §5).
    pub fn register_block_volume(&self, name: &str, image: impl AsRef<Path>) -> Result<Workspace> {
        validate_workspace_name(name)?;
        let image = image.as_ref();
        let canon = fs::canonicalize(image).map_err(|e| {
            SandboxError::InvalidConfig(format!("cannot register `{}`: {e}", image.display()))
        })?;
        if !canon.is_file() {
            return Err(SandboxError::InvalidConfig(format!(
                "block volume `{}` must be a regular file",
                canon.display()
            )));
        }
        self.inner.with_db(true, |db| {
            ensure_unique_name(db, name)?;
            let ws = new_workspace(
                WorkspaceId::new(),
                name,
                WorkspaceBacking::BlockVolume(canon),
            );
            db.workspaces.push(ws.clone());
            Ok(ws)
        })
    }

    pub fn get(&self, r: &WorkspaceRef) -> Result<Workspace> {
        let ws = self
            .inner
            .with_db(false, |db| find(db, r).map(|w| w.clone()))?;
        Ok(self.inner.decorate(ws))
    }

    /// All workspaces, sorted by name.
    pub fn list(&self) -> Result<Vec<Workspace>> {
        let mut all = self.inner.with_db(false, |db| Ok(db.workspaces.clone()))?;
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(all.into_iter().map(|w| self.inner.decorate(w)).collect())
    }

    /// Unregister a workspace. Refused while leased.
    ///
    /// With `delete_data`, a `ManagedDir` workspace's data is deleted; for other backings only
    /// Sandcrab's own per-workspace state (caches) is removed — a registered host directory
    /// or block volume is never deleted.
    pub fn remove(&self, r: &WorkspaceRef, delete_data: bool) -> Result<Workspace> {
        let ws = self.inner.with_db(true, |db| {
            let idx = db
                .workspaces
                .iter()
                .position(|w| r.matches(w))
                .ok_or_else(|| SandboxError::not_found("workspace", r))?;
            let id = db.workspaces[idx].id;
            if self.inner.is_leased(id) {
                return Err(SandboxError::WorkspaceBusy(format!(
                    "workspace `{}` is mounted by a running sandbox",
                    db.workspaces[idx].name
                )));
            }
            Ok(db.workspaces.remove(idx))
        })?;
        if delete_data {
            let dir = self.inner.workspace_dir(ws.id);
            match fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(SandboxError::io_at(&dir, e)),
            }
        }
        Ok(ws)
    }

    /// Set or clear the `DirtySession` flag (failed sync-back; see design §5).
    pub fn set_dirty(&self, r: &WorkspaceRef, dirty: bool) -> Result<Workspace> {
        self.inner.with_db(true, |db| {
            let ws = find(db, r)?;
            ws.state = if dirty {
                WorkspaceState::DirtySession
            } else {
                WorkspaceState::Idle
            };
            ws.updated_at = Utc::now();
            Ok(ws.clone())
        })
    }

    /// Bump the generation after an out-of-band change (e.g. a committed sync-back).
    pub fn bump_generation(&self, r: &WorkspaceRef) -> Result<u64> {
        self.inner.with_db(true, |db| {
            let ws = find(db, r)?;
            touch(ws);
            Ok(ws.generation)
        })
    }

    /// Write `bytes` to `rel_path`, creating parent directories. Bumps the generation.
    /// The write is atomic (temp file + rename in the target directory) and never follows
    /// a symlink.
    pub fn put_file(
        &self,
        r: &WorkspaceRef,
        rel_path: impl AsRef<Path>,
        bytes: &[u8],
    ) -> Result<()> {
        let rel = file_path(rel_path)?;
        self.inner.with_db(true, |db| {
            let ws = find(db, r)?;
            self.inner.ensure_mutable(ws)?;
            let root = fsx::open_root(&dir_root(ws)?)?;
            fsx::write_file(&root, &rel, bytes)?;
            touch(ws);
            Ok(())
        })
    }

    /// Read a regular file. Allowed under any lease; a symlink anywhere on the path is
    /// refused with [`SandboxError::PathEscape`].
    pub fn read_file(&self, r: &WorkspaceRef, rel_path: impl AsRef<Path>) -> Result<Vec<u8>> {
        let rel = file_path(rel_path)?;
        let ws = self
            .inner
            .with_db(false, |db| find(db, r).map(|w| w.clone()))?;
        let root = fsx::open_root(&dir_root(&ws)?)?;
        fsx::read_file(&root, &rel)
    }

    /// Delete a file, symlink (the link itself, never its target) or directory (recursively).
    pub fn delete_file(&self, r: &WorkspaceRef, rel_path: impl AsRef<Path>) -> Result<()> {
        let rel = file_path(rel_path)?;
        self.inner.with_db(true, |db| {
            let ws = find(db, r)?;
            self.inner.ensure_mutable(ws)?;
            let root = fsx::open_root(&dir_root(ws)?)?;
            fsx::delete(&root, &rel)?;
            touch(ws);
            Ok(())
        })
    }

    /// List one directory (non-recursive), sorted by path. Paths are relative to the root.
    /// Symlinks are listed as [`FileKind::Symlink`]; listing through one is refused.
    pub fn list_files(
        &self,
        r: &WorkspaceRef,
        rel_dir: impl AsRef<Path>,
    ) -> Result<Vec<FileEntry>> {
        let rel = SafeRelPath::new(rel_dir)?;
        let ws = self
            .inner
            .with_db(false, |db| find(db, r).map(|w| w.clone()))?;
        let root = fsx::open_root(&dir_root(&ws)?)?;
        let mut out: Vec<_> = fsx::list_dir(&root, &rel)?
            .into_iter()
            .map(|(name, kind, size)| FileEntry {
                path: rel.as_path().join(name),
                kind,
                size,
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Take a mount lease for sandbox `by`: read-write is exclusive (no other rw or ro
    /// lease), read-only is shared. Dropping the lease releases it; releasing a read-write
    /// lease bumps the generation (the guest may have changed the contents).
    pub fn lease(
        &self,
        r: &WorkspaceRef,
        mode: MountMode,
        by: SandboxId,
    ) -> Result<WorkspaceLease> {
        self.inner.with_db(false, |db| {
            let mut ws = find(db, r)?.clone();
            if ws.state == WorkspaceState::DirtySession {
                return Err(SandboxError::WorkspaceBusy(format!(
                    "workspace `{}` has a dirty session; recover or clear it first",
                    ws.name
                )));
            }
            let root = match &ws.backing {
                WorkspaceBacking::BlockVolume(p) => {
                    fs::canonicalize(p).map_err(|e| SandboxError::io_at(p, e))?
                }
                _ => dir_root(&ws)?,
            };
            let mut leases = lock(&self.inner.leases);
            let entry = leases.entry(ws.id).or_default();
            match mode {
                MountMode::ReadWrite => {
                    if let Some(holder) = entry.rw {
                        return Err(SandboxError::WorkspaceBusy(format!(
                            "workspace `{}` is already mounted read-write by sandbox {holder}",
                            ws.name
                        )));
                    }
                    if entry.ro > 0 {
                        return Err(SandboxError::WorkspaceBusy(format!(
                            "workspace `{}` is mounted read-only by {} sandbox(es); \
                             a read-write mount needs exclusive access",
                            ws.name, entry.ro
                        )));
                    }
                    entry.rw = Some(by);
                    ws.state = WorkspaceState::LeasedRw { sandbox: by };
                }
                MountMode::ReadOnly => {
                    if let Some(holder) = entry.rw {
                        return Err(SandboxError::WorkspaceBusy(format!(
                            "workspace `{}` is mounted read-write by sandbox {holder}",
                            ws.name
                        )));
                    }
                    entry.ro += 1;
                    ws.state = WorkspaceState::LeasedRo { holders: entry.ro };
                }
            }
            Ok(WorkspaceLease {
                inner: Arc::clone(&self.inner),
                generation: ws.generation,
                workspace: ws,
                mode,
                root,
                sandbox: by,
                failed: false,
            })
        })
    }
}

fn ensure_unique_name(db: &RegistryDb, name: &str) -> Result<()> {
    if db.workspaces.iter().any(|w| w.name == name) {
        return Err(SandboxError::AlreadyExists {
            kind: "workspace",
            id: name.to_owned(),
        });
    }
    Ok(())
}

fn new_workspace(id: WorkspaceId, name: &str, backing: WorkspaceBacking) -> Workspace {
    let now = Utc::now();
    Workspace {
        id,
        name: name.to_owned(),
        backing,
        generation: 0,
        quota_mb: None,
        state: WorkspaceState::Idle,
        created_at: now,
        updated_at: now,
    }
}

fn file_path(rel: impl AsRef<Path>) -> Result<SafeRelPath> {
    let rel = SafeRelPath::new(rel)?;
    if rel.is_root() {
        return Err(SandboxError::InvalidConfig(
            "a file path is required, got the workspace root".into(),
        ));
    }
    Ok(rel)
}

/// A mount lease on a workspace held by one sandbox. Dropping it releases the lease.
#[derive(Debug)]
pub struct WorkspaceLease {
    inner: Arc<Inner>,
    workspace: Workspace,
    mode: MountMode,
    root: PathBuf,
    generation: u64,
    sandbox: SandboxId,
    failed: bool,
}

impl WorkspaceLease {
    /// The workspace, with the state this lease put it in.
    #[must_use]
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    #[must_use]
    pub fn mode(&self) -> MountMode {
        self.mode
    }

    /// Canonical host path: the backing directory, or the image file for a block volume.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Generation at lease time (read-only image cache key).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn sandbox(&self) -> SandboxId {
        self.sandbox
    }

    /// Release after a failed read-write session (sync-back did not complete): the
    /// workspace is flagged `DirtySession` instead of getting a new generation.
    pub fn release_failed(mut self) {
        self.failed = true;
    }
}

impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        if self.mode.is_writable() {
            let id = self.workspace.id;
            let failed = self.failed;
            // Best effort: Drop cannot report errors.
            let _ = self.inner.with_db(true, |db| {
                if let Some(ws) = db.workspaces.iter_mut().find(|w| w.id == id) {
                    if failed {
                        ws.state = WorkspaceState::DirtySession;
                        ws.updated_at = Utc::now();
                    } else {
                        touch(ws);
                    }
                }
                Ok(())
            });
        }
        self.inner.release(self.workspace.id, self.mode);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        reg: WorkspaceRegistry,
        /// A directory outside both the state root and any workspace.
        outside: PathBuf,
        /// A registered host-dir workspace root.
        host: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let reg = WorkspaceRegistry::open(tmp.path().join("state")).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), b"top secret").unwrap();
        let host = tmp.path().join("project");
        fs::create_dir_all(&host).unwrap();
        reg.register_host_dir("proj", &host).unwrap();
        let outside = fs::canonicalize(outside).unwrap();
        let host = fs::canonicalize(host).unwrap();
        Fixture {
            _tmp: tmp,
            reg,
            outside,
            host,
        }
    }

    fn proj() -> WorkspaceRef {
        WorkspaceRef::from("proj")
    }

    #[test]
    fn create_get_list_remove_and_persist() {
        let f = fixture();
        let ws = f.reg.create("scratch").unwrap();
        assert!(matches!(&ws.backing, WorkspaceBacking::ManagedDir(p)
            if p.starts_with(f.reg.state_root()) && p.is_dir()));
        assert_eq!(f.reg.get(&"scratch".into()).unwrap().id, ws.id);
        assert_eq!(f.reg.get(&ws.id.into()).unwrap().name, "scratch");

        // Reopen: state persisted on disk.
        let reopened = WorkspaceRegistry::open(f.reg.state_root()).unwrap();
        let names: Vec<_> = reopened
            .list()
            .unwrap()
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert_eq!(names, ["proj", "scratch"]);

        assert!(matches!(
            f.reg.create("scratch"),
            Err(SandboxError::AlreadyExists { .. })
        ));
        assert!(matches!(
            f.reg.create("Bad Name"),
            Err(SandboxError::InvalidConfig(_))
        ));

        let data = ws.backing.path().to_path_buf();
        f.reg.remove(&"scratch".into(), true).unwrap();
        assert!(!data.exists());
        assert!(matches!(
            f.reg.get(&"scratch".into()),
            Err(SandboxError::NotFound { .. })
        ));
    }

    #[test]
    fn remove_never_deletes_host_dir() {
        let f = fixture();
        fs::write(f.host.join("keep"), b"x").unwrap();
        f.reg.remove(&proj(), true).unwrap();
        assert!(f.host.join("keep").exists());
    }

    #[test]
    fn remove_managed_without_delete_keeps_data() {
        let f = fixture();
        let ws = f.reg.create("keepme").unwrap();
        f.reg.remove(&"keepme".into(), false).unwrap();
        assert!(ws.backing.path().is_dir());
    }

    #[test]
    fn register_host_dir_guards() {
        let f = fixture();
        let state = f.reg.state_root().to_path_buf();
        let tmp_root = state.parent().unwrap().to_path_buf();
        for (path, why) in [
            (PathBuf::from("/"), "root"),
            (state.clone(), "state root"),
            (state.join("workspaces"), "inside state root"),
            (tmp_root.clone(), "ancestor of state root"),
            (tmp_root.join("missing"), "missing"),
            (f.outside.join("secret"), "file"),
        ] {
            assert!(
                matches!(
                    f.reg.register_host_dir("x", &path),
                    Err(SandboxError::InvalidConfig(_))
                ),
                "{why}"
            );
        }
        // Same directory twice (through a symlink alias) is refused.
        let alias = tmp_root.join("alias");
        symlink(&f.host, &alias).unwrap();
        assert!(matches!(
            f.reg.register_host_dir("again", &alias),
            Err(SandboxError::AlreadyExists { .. })
        ));
        // Canonicalized on registration.
        let ws = f.reg.get(&proj()).unwrap();
        assert_eq!(ws.backing, WorkspaceBacking::HostDir(f.host.clone()));
    }

    #[test]
    fn file_ops_roundtrip_and_generation() {
        let f = fixture();
        let g0 = f.reg.get(&proj()).unwrap().generation;
        f.reg
            .put_file(&proj(), "src/main.rs", b"fn main() {}")
            .unwrap();
        assert_eq!(
            fs::read(f.host.join("src/main.rs")).unwrap(),
            b"fn main() {}"
        );
        assert_eq!(
            f.reg.read_file(&proj(), "./src/main.rs").unwrap(),
            b"fn main() {}"
        );
        f.reg.put_file(&proj(), "README", b"hi").unwrap();
        let root = f.reg.list_files(&proj(), "").unwrap();
        assert_eq!(
            root.iter()
                .map(|e| (e.path.clone(), e.kind))
                .collect::<Vec<_>>(),
            [
                (PathBuf::from("README"), FileKind::File),
                (PathBuf::from("src"), FileKind::Dir)
            ]
        );
        let src = f.reg.list_files(&proj(), "src").unwrap();
        assert_eq!(src[0].path, PathBuf::from("src/main.rs"));
        assert_eq!(src[0].size, 12);

        f.reg.delete_file(&proj(), "src").unwrap();
        assert!(!f.host.join("src").exists());
        assert!(matches!(
            f.reg.read_file(&proj(), "src/main.rs"),
            Err(SandboxError::NotFound { .. })
        ));
        assert!(matches!(
            f.reg.delete_file(&proj(), "nope"),
            Err(SandboxError::NotFound { .. })
        ));
        assert_eq!(f.reg.get(&proj()).unwrap().generation, g0 + 3);
    }

    #[test]
    fn lexical_traversal_rejected() {
        let f = fixture();
        for bad in ["../outside/secret", "/etc/passwd", "a/../../x", "a/.."] {
            assert!(
                matches!(
                    f.reg.put_file(&proj(), bad, b"x"),
                    Err(SandboxError::PathEscape(_))
                ),
                "{bad}"
            );
            assert!(matches!(
                f.reg.read_file(&proj(), bad),
                Err(SandboxError::PathEscape(_))
            ));
        }
        assert!(matches!(
            f.reg.put_file(&proj(), "", b"x"),
            Err(SandboxError::InvalidConfig(_))
        ));
        assert!(matches!(
            f.reg.list_files(&proj(), "../"),
            Err(SandboxError::PathEscape(_))
        ));
    }

    #[test]
    fn symlinked_directory_escape_rejected() {
        let f = fixture();
        symlink(&f.outside, f.host.join("evil")).unwrap();
        assert!(matches!(
            f.reg.put_file(&proj(), "evil/planted", b"x"),
            Err(SandboxError::PathEscape(_))
        ));
        assert!(!f.outside.join("planted").exists());
        assert!(matches!(
            f.reg.read_file(&proj(), "evil/secret"),
            Err(SandboxError::PathEscape(_))
        ));
        assert!(matches!(
            f.reg.list_files(&proj(), "evil"),
            Err(SandboxError::PathEscape(_))
        ));
        assert!(matches!(
            f.reg.delete_file(&proj(), "evil/secret"),
            Err(SandboxError::PathEscape(_))
        ));
        assert!(f.outside.join("secret").exists());
    }

    #[test]
    fn relative_symlink_escape_rejected() {
        let f = fixture();
        fs::create_dir_all(f.host.join("a/b")).unwrap();
        symlink("../../..", f.host.join("a/b/up")).unwrap();
        assert!(matches!(
            f.reg.read_file(&proj(), "a/b/up/outside/secret"),
            Err(SandboxError::PathEscape(_))
        ));
        assert!(matches!(
            f.reg.put_file(&proj(), "a/b/up/outside/new", b"x"),
            Err(SandboxError::PathEscape(_))
        ));
        assert!(!f.outside.join("new").exists());
    }

    #[test]
    fn symlinked_file_escape_rejected() {
        let f = fixture();
        symlink(f.outside.join("secret"), f.host.join("passwd")).unwrap();
        assert!(matches!(
            f.reg.read_file(&proj(), "passwd"),
            Err(SandboxError::PathEscape(_))
        ));
        assert!(matches!(
            f.reg.put_file(&proj(), "passwd", b"overwritten"),
            Err(SandboxError::PathEscape(_))
        ));
        assert_eq!(fs::read(f.outside.join("secret")).unwrap(), b"top secret");
        // Deleting the link removes the link only.
        f.reg.delete_file(&proj(), "passwd").unwrap();
        assert!(fs::symlink_metadata(f.host.join("passwd")).is_err());
        assert_eq!(fs::read(f.outside.join("secret")).unwrap(), b"top secret");
    }

    #[test]
    fn dangling_symlink_escape_rejected() {
        let f = fixture();
        let target = f.outside.join("created-by-attacker");
        symlink(&target, f.host.join("dangling")).unwrap();
        assert!(matches!(
            f.reg.put_file(&proj(), "dangling", b"x"),
            Err(SandboxError::PathEscape(_))
        ));
        assert!(!target.exists());
        assert!(matches!(
            f.reg.put_file(&proj(), "dangling/child", b"x"),
            Err(SandboxError::PathEscape(_))
        ));
    }

    #[test]
    fn internal_symlinks_refused() {
        let f = fixture();
        fs::create_dir_all(f.host.join("real")).unwrap();
        fs::write(f.host.join("real/x.txt"), b"1").unwrap();
        symlink("real", f.host.join("alias")).unwrap();
        symlink(f.host.join("real"), f.host.join("abs_alias")).unwrap();
        symlink("real/x.txt", f.host.join("x_link")).unwrap();
        // Host-side ops never follow symlinks, even ones that stay inside the workspace.
        for bad in ["alias/y.txt", "abs_alias/x.txt", "x_link"] {
            assert!(
                matches!(
                    f.reg.put_file(&proj(), bad, b"2"),
                    Err(SandboxError::PathEscape(_))
                ),
                "{bad}"
            );
            assert!(
                matches!(
                    f.reg.read_file(&proj(), bad),
                    Err(SandboxError::PathEscape(_))
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            f.reg.list_files(&proj(), "alias"),
            Err(SandboxError::PathEscape(_))
        ));
        assert!(!f.host.join("real/y.txt").exists());
        assert_eq!(fs::read(f.host.join("real/x.txt")).unwrap(), b"1");
        let listed = f.reg.list_files(&proj(), "").unwrap();
        assert!(
            listed
                .iter()
                .any(|e| e.path == Path::new("alias") && e.kind == FileKind::Symlink)
        );
        // Deleting a symlinked directory removes the link, not the directory.
        f.reg.delete_file(&proj(), "alias").unwrap();
        assert!(f.host.join("real/x.txt").exists());
    }

    #[test]
    fn leases() {
        let f = fixture();
        let a = SandboxId::new();
        let b = SandboxId::new();
        let g0 = f.reg.get(&proj()).unwrap().generation;

        let rw = f.reg.lease(&proj(), MountMode::ReadWrite, a).unwrap();
        assert_eq!(rw.root(), f.host);
        assert_eq!(
            f.reg.get(&proj()).unwrap().state,
            WorkspaceState::LeasedRw { sandbox: a }
        );
        assert!(matches!(
            f.reg.lease(&proj(), MountMode::ReadWrite, b),
            Err(SandboxError::WorkspaceBusy(_))
        ));
        assert!(matches!(
            f.reg.lease(&proj(), MountMode::ReadOnly, b),
            Err(SandboxError::WorkspaceBusy(_))
        ));
        assert!(matches!(
            f.reg.put_file(&proj(), "x", b"x"),
            Err(SandboxError::WorkspaceBusy(_))
        ));
        assert!(matches!(
            f.reg.delete_file(&proj(), "x"),
            Err(SandboxError::WorkspaceBusy(_))
        ));
        assert!(matches!(
            f.reg.remove(&proj(), false),
            Err(SandboxError::WorkspaceBusy(_))
        ));
        // Reads are fine under a lease.
        f.reg.list_files(&proj(), "").unwrap();
        drop(rw);
        let ws = f.reg.get(&proj()).unwrap();
        assert_eq!(ws.state, WorkspaceState::Idle);
        assert_eq!(ws.generation, g0 + 1, "rw release bumps generation");

        let r1 = f.reg.lease(&proj(), MountMode::ReadOnly, a).unwrap();
        let r2 = f.reg.lease(&proj(), MountMode::ReadOnly, b).unwrap();
        assert_eq!(
            f.reg.get(&proj()).unwrap().state,
            WorkspaceState::LeasedRo { holders: 2 }
        );
        assert!(matches!(
            f.reg.lease(&proj(), MountMode::ReadWrite, a),
            Err(SandboxError::WorkspaceBusy(_))
        ));
        // Host writes are allowed while only read-only leases exist.
        f.reg.put_file(&proj(), "y", b"y").unwrap();
        drop((r1, r2));
        assert_eq!(f.reg.get(&proj()).unwrap().state, WorkspaceState::Idle);
        f.reg.lease(&proj(), MountMode::ReadWrite, a).unwrap();
    }

    #[test]
    fn failed_rw_session_marks_dirty() {
        let f = fixture();
        let lease = f
            .reg
            .lease(&proj(), MountMode::ReadWrite, SandboxId::new())
            .unwrap();
        lease.release_failed();
        assert_eq!(
            f.reg.get(&proj()).unwrap().state,
            WorkspaceState::DirtySession
        );
        assert!(matches!(
            f.reg.lease(&proj(), MountMode::ReadOnly, SandboxId::new()),
            Err(SandboxError::WorkspaceBusy(_))
        ));
        assert!(matches!(
            f.reg.put_file(&proj(), "x", b"x"),
            Err(SandboxError::WorkspaceBusy(_))
        ));
        f.reg.set_dirty(&proj(), false).unwrap();
        f.reg.put_file(&proj(), "x", b"x").unwrap();
    }

    #[test]
    fn block_volume_host_ops_unimplemented() {
        let f = fixture();
        let img = f.outside.join("vol.ext4");
        fs::write(&img, b"").unwrap();
        f.reg.register_block_volume("vol", &img).unwrap();
        assert!(matches!(
            f.reg.put_file(&"vol".into(), "x", b"x"),
            Err(SandboxError::Unimplemented(_))
        ));
        let lease = f
            .reg
            .lease(&"vol".into(), MountMode::ReadWrite, SandboxId::new())
            .unwrap();
        assert_eq!(lease.root(), img);
    }

    #[test]
    fn corrupt_registry_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(REGISTRY_FILE), b"{not json").unwrap();
        assert!(matches!(
            WorkspaceRegistry::open(tmp.path()),
            Err(SandboxError::Workspace(_))
        ));
    }

    #[test]
    fn concurrent_creates_are_serialized() {
        let f = fixture();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let reg = f.reg.clone();
                std::thread::spawn(move || reg.create(&format!("ws{i}")).unwrap())
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // A second registry instance on the same root (another "process") sees all of them.
        let other = WorkspaceRegistry::open(f.reg.state_root()).unwrap();
        assert_eq!(other.list().unwrap().len(), 9);
    }
}
