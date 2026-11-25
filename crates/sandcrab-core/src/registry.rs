//! Persistent workspace registry with host-side file operations and mount leases.
//!
//! State layout under the state root (default `~/.sandcrab/`, `$SANDCRAB_HOME` overrides):
//!
//! ```text
//! <root>/registry.json            # all workspaces (atomic write: tmp + fsync + rename)
//! <root>/registry.lock            # flock held for every registry read-modify-write
//! <root>/workspaces/<id>/data/    # ManagedDir contents
//! <root>/workspaces/<id>/...      # per-workspace backend state (ro image cache, staging, recovery)
//! <root>/locks/<id>.mount.lock    # flock: rw lease exclusive, ro leases shared
//! <root>/locks/<id>.rw.lock       # flock: held exclusive by the rw lease
//! ```
//!
//! Lease lock files live in `locks/`, not in the workspace dir, so leasing a registered
//! host directory (Docker bind-mounts it in place) creates no `workspaces/<id>/` dir.
//! [`WorkspaceRegistry::remove`] deletes them.
//!
//! # Host-side file operations
//!
//! `put_file` / `read_file` / `delete_file` / `list_files` treat the workspace contents as
//! hostile (a sandbox may be rewriting them concurrently). They refuse `..` and absolute
//! paths, and walk fd-relative from an opened root handle without following any symlink
//! (see [`crate::fsx`]): a symlink anywhere on the path — even one pointing inside the
//! workspace — is refused with [`SandboxError::PathEscape`]. `list_files` reports symlinks
//! and `delete_file` removes the link itself; neither follows it.
//!
//! # Workspace roots
//!
//! Directory roots are stored canonical at registration together with their `(dev, ino)`
//! ([`RootIdentity`]). Every host op and lease opens the stored path by walking from `/`
//! with `O_NOFOLLOW | O_DIRECTORY` per component — never `canonicalize` again — so a root
//! (or any ancestor) later replaced by a symlink, removed, or swapped for another directory
//! fails with [`SandboxError::WorkspaceRootChanged`]. Workspace roots may not overlap each
//! other or the state root (equal, ancestor or descendant): otherwise a sandbox holding
//! the outer workspace read-write could swap the inner one's root. Registries written
//! before this rule are tolerated, but overlapping workspaces cannot be leased.
//!
//! # Leases
//!
//! Mount leases are enforced across processes with `flock` on the per-workspace lock files
//! above (plus in-process bookkeeping for state and messages). A read-write lease is
//! exclusive against every other lease; read-only leases share. While a read-write lease
//! is held (by any process), host mutation (`put_file`, `delete_file`, `remove`) fails with
//! [`SandboxError::WorkspaceBusy`] unless the caller opts in with
//! [`HostWriteOptions::force`]. Reads are always allowed; they stay race-safe through the
//! fd-relative walk. Leases held by another process are not reflected in
//! [`Workspace::state`] (which shows this process's view).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use std::os::fd::OwnedFd;

use crate::fsx;
use crate::workspace::validate_workspace_name;
use crate::{
    MountMode, Result, RootIdentity, SafeRelPath, SandboxError, SandboxId, Workspace,
    WorkspaceBacking, WorkspaceId, WorkspaceRef, WorkspaceState,
};

/// Environment variable overriding the default state root.
pub const STATE_ROOT_ENV: &str = "SANDCRAB_HOME";

const REGISTRY_FILE: &str = "registry.json";
const LOCK_FILE: &str = "registry.lock";
const REGISTRY_VERSION: u32 = 1;
/// Per-workspace flock: exclusive for a rw lease, shared for ro leases.
const MOUNT_LOCK: &str = "mount.lock";
/// Per-workspace flock held exclusively by a rw lease; host mutation probes it.
const RW_LOCK: &str = "rw.lock";
/// Lease lock files: `<root>/locks/<id>.{mount,rw}.lock`.
const LOCKS_DIR: &str = "locks";
/// `ManagedDir` contents under `<root>/workspaces/<id>/`.
const MANAGED_DATA: &str = "data";

/// Default cap for [`WorkspaceRegistry::read_file`] (64 MiB).
pub const DEFAULT_READ_LIMIT: u64 = 64 * 1024 * 1024;

/// Options for host-side mutation (`put_file_with`, `delete_file_with`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostWriteOptions {
    /// Mutate even while a sandbox holds the workspace read-write (the guest may observe
    /// a half-applied change or overwrite it). Path safety is unaffected; a dirty session
    /// is still refused.
    pub force: bool,
}

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

    /// Refuse host-side mutation while a sandbox (in any process) can write, unless
    /// `force`, or while a session is dirty. The returned guard holds `rw.lock` for the
    /// duration of the mutation. Must be called under the exclusive registry lock (lease
    /// acquisition takes the shared one), so probing the lease locks cannot race a lease.
    fn ensure_mutable(&self, ws: &Workspace, opts: HostWriteOptions) -> Result<Option<File>> {
        if ws.state == WorkspaceState::DirtySession {
            return Err(SandboxError::WorkspaceBusy(format!(
                "workspace `{}` has a dirty session; recover or clear it first",
                ws.name
            )));
        }
        if opts.force {
            return Ok(None);
        }
        if let Some(sandbox) = lock(&self.leases).get(&ws.id).and_then(|l| l.rw) {
            return Err(SandboxError::WorkspaceBusy(format!(
                "workspace `{}` is mounted read-write by sandbox {sandbox}",
                ws.name
            )));
        }
        let file = self.lock_file(ws.id, RW_LOCK)?;
        if !try_flock(&file, true, &self.lock_path(ws.id, RW_LOCK))? {
            return Err(SandboxError::WorkspaceBusy(format!(
                "workspace `{}` is mounted read-write by a sandbox in another process",
                ws.name
            )));
        }
        Ok(Some(file))
    }

    fn lock_dir(&self) -> PathBuf {
        self.root.join(LOCKS_DIR)
    }

    fn lock_path(&self, id: WorkspaceId, name: &str) -> PathBuf {
        self.lock_dir().join(format!("{id}.{name}"))
    }

    /// Open (creating) a per-workspace lock file.
    fn lock_file(&self, id: WorkspaceId, name: &str) -> Result<File> {
        create_private_dir_all(&self.lock_dir())?;
        let path = self.lock_path(id, name);
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| SandboxError::io_at(&path, e))
    }

    /// Whether any process holds any lease on `id` (probe under the exclusive registry lock).
    fn is_leased_anywhere(&self, id: WorkspaceId) -> Result<bool> {
        if self.is_leased(id) {
            return Ok(true);
        }
        let path = self.lock_path(id, MOUNT_LOCK);
        let file = match OpenOptions::new().write(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(SandboxError::io_at(&path, e)),
        };
        Ok(!try_flock(&file, true, &path)?)
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

/// Non-blocking `flock`: `Ok(false)` when another open file description holds it.
fn try_flock(file: &File, exclusive: bool, path: &Path) -> Result<bool> {
    let res = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    match res {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(e)) => Err(SandboxError::io_at(path, e)),
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

/// Open the stored root of a directory-backed workspace without following any symlink
/// (see the module docs) and check its identity. Returns the handle and observed identity.
fn open_verified(
    name: &str,
    path: &Path,
    expected: Option<RootIdentity>,
) -> Result<(OwnedFd, RootIdentity)> {
    let changed = |why: String| {
        SandboxError::WorkspaceRootChanged(format!(
            "workspace `{name}` root {}: {why}; it is no longer the registered directory, \
             refusing to use it (if this is intended, `workspace rm` and register it again)",
            path.display()
        ))
    };
    let fd = match fsx::open_root_nofollow(path) {
        Ok(fd) => fd,
        Err(fsx::RootOpenError::Changed(why)) => return Err(changed(why)),
        Err(fsx::RootOpenError::Io(e)) => return Err(SandboxError::io_at(path, e)),
    };
    let (dev, ino) = fsx::identity(&fd).map_err(|e| SandboxError::io_at(path, e))?;
    let got = RootIdentity { dev, ino };
    if let Some(want) = expected
        && want != got
    {
        return Err(changed(format!(
            "device/inode changed ({}:{} at registration, now {}:{})",
            want.dev, want.ino, got.dev, got.ino
        )));
    }
    Ok((fd, got))
}

/// Re-check, right before use, that `path` (a workspace root from a lease or
/// [`crate::ResolvedMount`]) is still a symlink-free path to the directory with identity
/// `expected`. Backends call this immediately before bind-mounting, packing or syncing back.
/// Errors with [`SandboxError::WorkspaceRootChanged`] otherwise.
pub fn verify_workspace_root(
    name: &str,
    path: &Path,
    expected: Option<RootIdentity>,
) -> Result<RootIdentity> {
    open_verified(name, path, expected).map(|(_, id)| id)
}

/// Open the root of a directory-backed workspace (verified, see [`open_verified`]).
fn open_ws_root(ws: &Workspace) -> Result<OwnedFd> {
    match &ws.backing {
        WorkspaceBacking::HostDir(p) | WorkspaceBacking::ManagedDir(p) => {
            open_verified(&ws.name, p, ws.root_identity).map(|(fd, _)| fd)
        }
        WorkspaceBacking::BlockVolume(_) => {
            Err(SandboxError::Unimplemented("block volume host mutation"))
        }
    }
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Refuse a new directory root `canon` that is equal to, inside, or contains an existing
/// directory-backed workspace.
fn ensure_no_overlap(db: &RegistryDb, canon: &Path) -> Result<()> {
    for w in db.workspaces.iter().filter(|w| w.backing.is_dir()) {
        let p = w.backing.path();
        if p == canon {
            return Err(SandboxError::AlreadyExists {
                kind: "host dir workspace",
                id: format!("{} (registered as `{}`)", canon.display(), w.name),
            });
        }
        if canon.starts_with(p) {
            return Err(SandboxError::InvalidConfig(format!(
                "refusing to register `{}`: it is inside workspace `{}` ({}); nested \
                 workspaces are not allowed (a sandbox with the outer one mounted read-write \
                 could swap the inner root). Mount `{}` instead, at a different guest path \
                 if needed",
                canon.display(),
                w.name,
                p.display(),
                w.name
            )));
        }
        if p.starts_with(canon) {
            return Err(SandboxError::InvalidConfig(format!(
                "refusing to register `{}`: it contains workspace `{}` ({}); nested \
                 workspaces are not allowed. Remove it first with `sandcrab workspace rm {}`",
                canon.display(),
                w.name,
                p.display(),
                w.name
            )));
        }
    }
    Ok(())
}

/// For registries written before overlaps were refused: the first other workspace whose
/// root overlaps `ws`'s (or the state root, for a host dir), as an error.
fn legacy_overlap(db: &RegistryDb, ws: &Workspace, state_root: &Path) -> Result<()> {
    if !ws.backing.is_dir() {
        return Ok(());
    }
    let mine = ws.backing.path();
    if let Some(other) = db
        .workspaces
        .iter()
        .filter(|w| w.id != ws.id && w.backing.is_dir())
        .find(|w| paths_overlap(mine, w.backing.path()))
    {
        return Err(SandboxError::Workspace(format!(
            "workspace `{}` ({}) overlaps workspace `{}` ({}); overlapping workspaces cannot \
             be mounted. Run `sandcrab workspace rm {}` (or `{}`) to fix the registry",
            ws.name,
            mine.display(),
            other.name,
            other.backing.path().display(),
            other.name,
            ws.name
        )));
    }
    if matches!(ws.backing, WorkspaceBacking::HostDir(_)) && paths_overlap(mine, state_root) {
        return Err(SandboxError::Workspace(format!(
            "workspace `{}` ({}) overlaps the Sandcrab state root {}; it cannot be mounted. \
             Run `sandcrab workspace rm {}`",
            ws.name,
            mine.display(),
            state_root.display(),
            ws.name
        )));
    }
    Ok(())
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
            let data = self.inner.workspace_dir(id).join(MANAGED_DATA);
            ensure_no_overlap(db, &data)?;
            create_private_dir_all(&data)?;
            let (_, identity) = open_verified(name, &data, None)?;
            let mut ws = new_workspace(id, name, WorkspaceBacking::ManagedDir(data));
            ws.root_identity = Some(identity);
            db.workspaces.push(ws.clone());
            Ok(ws)
        })?;
        Ok(ws)
    }

    /// Register an existing host directory as a workspace. The path is canonicalized; `/`,
    /// the state root, and anything overlapping it are refused, as is a directory that is
    /// already registered ([`SandboxError::AlreadyExists`]) or that is inside / contains
    /// another workspace ([`SandboxError::InvalidConfig`] naming it). Its `(dev, ino)` is
    /// recorded.
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
            ensure_no_overlap(db, &canon)?;
            let (_, identity) = open_verified(name, &canon, None)?;
            let mut ws = new_workspace(WorkspaceId::new(), name, WorkspaceBacking::HostDir(canon));
            ws.root_identity = Some(identity);
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
    /// Sandcrab's own per-workspace state (`<root>/workspaces/<id>/`: lock files, caches,
    /// staging) is always removed. With `delete_data`, a `ManagedDir` workspace's data goes
    /// too; without it the data directory is kept. A registered host directory or block
    /// volume is never touched.
    pub fn remove(&self, r: &WorkspaceRef, delete_data: bool) -> Result<Workspace> {
        let ws = self.inner.with_db(true, |db| {
            let idx = db
                .workspaces
                .iter()
                .position(|w| r.matches(w))
                .ok_or_else(|| SandboxError::not_found("workspace", r))?;
            let id = db.workspaces[idx].id;
            if self.inner.is_leased_anywhere(id)? {
                return Err(SandboxError::WorkspaceBusy(format!(
                    "workspace `{}` is mounted by a running sandbox",
                    db.workspaces[idx].name
                )));
            }
            // Under the exclusive registry lock, and the workspace is not leased: nobody
            // holds these locks, and nobody can lease it once it is unregistered.
            for name in [MOUNT_LOCK, RW_LOCK] {
                let path = self.inner.lock_path(id, name);
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(SandboxError::io_at(&path, e)),
                }
            }
            Ok(db.workspaces.remove(idx))
        })?;
        let dir = self.inner.workspace_dir(ws.id);
        let keep_data = !delete_data && matches!(ws.backing, WorkspaceBacking::ManagedDir(_));
        let res = if keep_data {
            remove_state_except(&dir, MANAGED_DATA)
        } else {
            fs::remove_dir_all(&dir)
        };
        match res {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(SandboxError::io_at(&dir, e)),
        }
        Ok(ws)
    }

    /// Set or clear the `DirtySession` flag (failed sync-back; see design §5).
    pub fn set_dirty(&self, r: &WorkspaceRef, dirty: bool) -> Result<Workspace> {
        self.inner.with_db(true, |db| {
            let ws = find(db, r)?;
            refresh_managed_identity(ws);
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
    /// Refused while a sandbox (in any process) holds the workspace read-write; see
    /// [`Self::put_file_with`] to override.
    pub fn put_file(
        &self,
        r: &WorkspaceRef,
        rel_path: impl AsRef<Path>,
        bytes: &[u8],
    ) -> Result<()> {
        self.put_file_with(r, rel_path, bytes, HostWriteOptions::default())
    }

    /// [`Self::put_file`] with options. The write is atomic (temp file + rename in the
    /// target directory) and never follows a symlink.
    pub fn put_file_with(
        &self,
        r: &WorkspaceRef,
        rel_path: impl AsRef<Path>,
        bytes: &[u8],
        opts: HostWriteOptions,
    ) -> Result<()> {
        let rel = file_path(rel_path)?;
        self.inner.with_db(true, |db| {
            let ws = find(db, r)?;
            let _guard = self.inner.ensure_mutable(ws, opts)?;
            let root = open_ws_root(ws)?;
            fsx::write_file(&root, &rel, bytes)?;
            touch(ws);
            Ok(())
        })
    }

    /// Read a regular file of at most [`DEFAULT_READ_LIMIT`] bytes (else
    /// [`SandboxError::TooLarge`]; see [`Self::read_file_limited`] and [`Self::open_file`]).
    /// Allowed under any lease; a symlink anywhere on the path is refused with
    /// [`SandboxError::PathEscape`].
    pub fn read_file(&self, r: &WorkspaceRef, rel_path: impl AsRef<Path>) -> Result<Vec<u8>> {
        self.read_file_limited(r, rel_path, DEFAULT_READ_LIMIT)
    }

    /// [`Self::read_file`] with an explicit size limit in bytes.
    pub fn read_file_limited(
        &self,
        r: &WorkspaceRef,
        rel_path: impl AsRef<Path>,
        limit: u64,
    ) -> Result<Vec<u8>> {
        let rel = file_path(rel_path)?;
        let ws = self
            .inner
            .with_db(false, |db| find(db, r).map(|w| w.clone()))?;
        fsx::read_file(&open_ws_root(&ws)?, &rel, limit)
    }

    /// Open a regular file for streaming: `(reader, size at open)`. Same path rules as
    /// [`Self::read_file`], no size limit.
    pub fn open_file(&self, r: &WorkspaceRef, rel_path: impl AsRef<Path>) -> Result<(File, u64)> {
        let rel = file_path(rel_path)?;
        let ws = self
            .inner
            .with_db(false, |db| find(db, r).map(|w| w.clone()))?;
        fsx::open_file(&open_ws_root(&ws)?, &rel)
    }

    /// Delete a file, symlink (the link itself, never its target) or directory (recursively).
    pub fn delete_file(&self, r: &WorkspaceRef, rel_path: impl AsRef<Path>) -> Result<()> {
        self.delete_file_with(r, rel_path, HostWriteOptions::default())
    }

    /// [`Self::delete_file`] with options.
    pub fn delete_file_with(
        &self,
        r: &WorkspaceRef,
        rel_path: impl AsRef<Path>,
        opts: HostWriteOptions,
    ) -> Result<()> {
        let rel = file_path(rel_path)?;
        self.inner.with_db(true, |db| {
            let ws = find(db, r)?;
            let _guard = self.inner.ensure_mutable(ws, opts)?;
            let root = open_ws_root(ws)?;
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
        let root = open_ws_root(&ws)?;
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
    /// lease), read-only is shared — across processes, via `flock` on the workspace's lock
    /// files. Dropping the lease releases it; releasing a read-write lease bumps the
    /// generation (the guest may have changed the contents).
    ///
    /// A directory root is opened without following symlinks and its `(dev, ino)` checked
    /// ([`SandboxError::WorkspaceRootChanged`] otherwise); the lease carries the verified
    /// path and identity for the backend to re-check right before use. A workspace whose
    /// root overlaps another's (legacy registries) is refused.
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
            legacy_overlap(db, &ws, &self.inner.root)?;
            let (root, root_identity) = match &ws.backing {
                WorkspaceBacking::BlockVolume(p) => (
                    fs::canonicalize(p).map_err(|e| SandboxError::io_at(p, e))?,
                    None,
                ),
                WorkspaceBacking::HostDir(p) | WorkspaceBacking::ManagedDir(p) => {
                    let (_, id) = open_verified(&ws.name, p, ws.root_identity)?;
                    (p.clone(), Some(id))
                }
            };
            let mut leases = lock(&self.inner.leases);
            let entry = leases.entry(ws.id).or_default();
            let elsewhere = |what: &str| {
                SandboxError::WorkspaceBusy(format!(
                    "workspace `{}` is mounted {what} by a sandbox in another process",
                    ws.name
                ))
            };
            let mut locks = Vec::new();
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
                    for name in [MOUNT_LOCK, RW_LOCK] {
                        let file = self.inner.lock_file(ws.id, name)?;
                        if !try_flock(&file, true, &self.inner.lock_path(ws.id, name))? {
                            return Err(elsewhere("(read-write or read-only)"));
                        }
                        locks.push(file);
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
                    let file = self.inner.lock_file(ws.id, MOUNT_LOCK)?;
                    if !try_flock(&file, false, &self.inner.lock_path(ws.id, MOUNT_LOCK))? {
                        return Err(elsewhere("read-write"));
                    }
                    locks.push(file);
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
                root_identity,
                sandbox: by,
                failed: false,
                _locks: locks,
            })
        })
    }
}

/// A `ManagedDir` is legitimately replaced by Sandcrab itself (Firecracker sync-back swaps
/// the committed tree in with `RENAME_EXCHANGE`), so its pinned identity is re-recorded when
/// a rw session ends or its dirty flag changes. It lives in the private state root, out of
/// any sandbox's reach; the symlink-free walk still applies. Host dirs are never re-pinned.
fn refresh_managed_identity(ws: &mut Workspace) {
    if let WorkspaceBacking::ManagedDir(p) = &ws.backing
        && let Ok(fd) = fsx::open_root_nofollow(p)
        && let Ok((dev, ino)) = fsx::identity(&fd)
    {
        ws.root_identity = Some(RootIdentity { dev, ino });
    }
}

/// Remove everything in `dir` except the entry `keep`.
fn remove_state_except(dir: &Path, keep: &str) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == keep {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
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
        root_identity: None,
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
    root_identity: Option<RootIdentity>,
    generation: u64,
    sandbox: SandboxId,
    failed: bool,
    /// Held `flock`s (`mount.lock`, plus `rw.lock` for rw); released after `Drop::drop`
    /// has recorded the release in the registry.
    _locks: Vec<File>,
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

    /// Verified host path: the backing directory (opened symlink-free at lease time), or
    /// the image file for a block volume.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `(dev, ino)` of the directory root observed at lease time (`None` for a block volume).
    #[must_use]
    pub fn root_identity(&self) -> Option<RootIdentity> {
        self.root_identity
    }

    /// Re-check that [`Self::root`] is still the leased directory (see
    /// [`verify_workspace_root`]). No-op for a block volume.
    pub fn verify_root(&self) -> Result<()> {
        if self.workspace.backing.is_dir() {
            verify_workspace_root(&self.workspace.name, &self.root, self.root_identity)?;
        }
        Ok(())
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
                    refresh_managed_identity(ws);
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
        fs::write(ws.backing.path().join("file"), b"x").unwrap();
        drop(
            f.reg
                .lease(&"keepme".into(), MountMode::ReadWrite, SandboxId::new())
                .unwrap(),
        );
        let dir = f.reg.workspace_dir(ws.id);
        let lock = f.reg.inner.lock_path(ws.id, MOUNT_LOCK);
        assert!(lock.exists());
        fs::write(dir.join("cache"), b"state").unwrap();
        f.reg.remove(&"keepme".into(), false).unwrap();
        assert!(!lock.exists());
        assert!(ws.backing.path().join("file").is_file());
        // Sandcrab's own state (lock files, caches) is cleaned; only `data/` stays.
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [MANAGED_DATA]);
    }

    #[test]
    fn remove_host_dir_cleans_state_only() {
        let f = fixture();
        fs::write(f.host.join("keep"), b"x").unwrap();
        let id = f.reg.get(&proj()).unwrap().id;
        drop(
            f.reg
                .lease(&proj(), MountMode::ReadWrite, SandboxId::new())
                .unwrap(),
        );
        // Leasing a host dir creates no per-workspace dir: its locks live in `locks/`.
        assert!(!f.reg.workspace_dir(id).exists());
        let locks = [MOUNT_LOCK, RW_LOCK].map(|n| f.reg.inner.lock_path(id, n));
        assert!(locks.iter().all(|l| l.exists()), "{locks:?}");
        // A ro lease and a host write do not create it either.
        drop(
            f.reg
                .lease(&proj(), MountMode::ReadOnly, SandboxId::new())
                .unwrap(),
        );
        f.reg.put_file(&proj(), "new", b"y").unwrap();
        assert!(!f.reg.workspace_dir(id).exists());
        f.reg.remove(&proj(), false).unwrap();
        assert!(!f.reg.workspace_dir(id).exists());
        assert!(locks.iter().all(|l| !l.exists()), "{locks:?}");
        assert_eq!(fs::read(f.host.join("keep")).unwrap(), b"x");
    }

    #[test]
    fn overlapping_registrations_refused() {
        let f = fixture();
        let tmp_root = f.host.parent().unwrap().to_path_buf();
        fs::create_dir_all(f.host.join("inner/deeper")).unwrap();
        let outer_parent = tmp_root.join("wrap");
        fs::create_dir_all(&outer_parent).unwrap();
        // Equal (also through a `.` alias): AlreadyExists naming `proj`.
        match f.reg.register_host_dir("eq", f.host.join(".")) {
            Err(SandboxError::AlreadyExists { id, .. }) => assert!(id.contains("proj"), "{id}"),
            other => panic!("equal: {other:?}"),
        }
        // Descendant of an existing workspace.
        match f
            .reg
            .register_host_dir("inner", f.host.join("inner/deeper"))
        {
            Err(SandboxError::InvalidConfig(m)) => {
                assert!(m.contains("inside workspace `proj`"), "{m}");
            }
            other => panic!("descendant: {other:?}"),
        }
        // Ancestor of an existing workspace.
        match f.reg.register_host_dir("anc", &tmp_root) {
            Err(SandboxError::InvalidConfig(m)) => assert!(m.contains("overlaps"), "{m}"),
            other => panic!("ancestor (also of the state root): {other:?}"),
        }
        let sub = f.outside.join("ws");
        fs::create_dir_all(sub.join("x")).unwrap();
        f.reg.register_host_dir("sub", &sub).unwrap();
        match f.reg.register_host_dir("anc2", &f.outside) {
            Err(SandboxError::InvalidConfig(m)) => {
                assert!(m.contains("contains workspace `sub`"), "{m}");
            }
            other => panic!("ancestor: {other:?}"),
        }
        assert!(matches!(
            f.reg.register_host_dir("desc2", sub.join("x")),
            Err(SandboxError::InvalidConfig(_))
        ));
        // State dir and everything in it (including managed data dirs).
        let managed = f.reg.create("m").unwrap();
        for bad in [
            f.reg.state_root().to_path_buf(),
            managed.backing.path().to_path_buf(),
        ] {
            assert!(
                matches!(
                    f.reg.register_host_dir("st", &bad),
                    Err(SandboxError::InvalidConfig(_))
                ),
                "{}",
                bad.display()
            );
        }
        // Disjoint siblings are fine, and managed workspaces record their identity.
        f.reg.register_host_dir("wrap", &outer_parent).unwrap();
        assert!(managed.root_identity.is_some());
        assert!(f.reg.get(&proj()).unwrap().root_identity.is_some());
    }

    fn assert_root_changed<T: std::fmt::Debug>(r: Result<T>, what: &str) {
        assert!(
            matches!(r, Err(SandboxError::WorkspaceRootChanged(_))),
            "{what}: {r:?}"
        );
    }

    /// Every host op and lease on `proj` must refuse, and `outside` must be untouched.
    fn assert_all_ops_refused(f: &Fixture) {
        assert_root_changed(f.reg.read_file(&proj(), "secret"), "read");
        assert_root_changed(f.reg.open_file(&proj(), "secret"), "open");
        assert_root_changed(f.reg.put_file(&proj(), "planted", b"x"), "put");
        assert_root_changed(f.reg.delete_file(&proj(), "secret"), "delete");
        assert_root_changed(f.reg.list_files(&proj(), ""), "list");
        for mode in [MountMode::ReadWrite, MountMode::ReadOnly] {
            assert_root_changed(f.reg.lease(&proj(), mode, SandboxId::new()), "lease");
        }
        assert_eq!(fs::read(f.outside.join("secret")).unwrap(), b"top secret");
        assert!(!f.outside.join("planted").exists());
    }

    #[test]
    fn root_replaced_by_symlink_refused() {
        let f = fixture();
        fs::remove_dir(&f.host).unwrap();
        symlink(&f.outside, &f.host).unwrap();
        assert_all_ops_refused(&f);
    }

    #[test]
    fn root_ancestor_replaced_by_symlink_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let reg = WorkspaceRegistry::open(base.join("state")).unwrap();
        fs::create_dir_all(base.join("a/ws")).unwrap();
        fs::create_dir_all(base.join("evil/ws")).unwrap();
        fs::write(base.join("evil/ws/secret"), b"victim").unwrap();
        reg.register_host_dir("w", base.join("a/ws")).unwrap();
        fs::rename(base.join("a"), base.join("a-old")).unwrap();
        symlink(base.join("evil"), base.join("a")).unwrap();
        let w = WorkspaceRef::from("w");
        assert_root_changed(reg.read_file(&w, "secret"), "read");
        assert_root_changed(
            reg.lease(&w, MountMode::ReadOnly, SandboxId::new()),
            "lease",
        );
        assert_root_changed(reg.delete_file(&w, "secret"), "delete");
        assert!(base.join("evil/ws/secret").exists());
    }

    #[test]
    fn root_dev_ino_mismatch_refused() {
        let f = fixture();
        let lease = f
            .reg
            .lease(&proj(), MountMode::ReadOnly, SandboxId::new())
            .unwrap();
        lease.verify_root().unwrap();
        // Same path, different directory (renamed away, a new one created in its place).
        fs::rename(&f.host, f.host.with_file_name("project-old")).unwrap();
        fs::create_dir(&f.host).unwrap();
        fs::write(f.host.join("secret"), b"impostor").unwrap();
        assert_root_changed(lease.verify_root(), "verify after swap");
        drop(lease);
        assert_all_ops_refused(&f);
        assert_root_changed(
            verify_workspace_root("proj", &f.host, f.reg.get(&proj()).unwrap().root_identity),
            "verify_workspace_root",
        );
        // Missing root.
        fs::remove_dir_all(&f.host).unwrap();
        assert_root_changed(f.reg.list_files(&proj(), ""), "missing");
    }

    #[test]
    fn lease_root_reverify_catches_symlink_swap() {
        let f = fixture();
        let lease = f
            .reg
            .lease(&proj(), MountMode::ReadWrite, SandboxId::new())
            .unwrap();
        assert_eq!(lease.root(), f.host);
        assert!(lease.root_identity().is_some());
        fs::rename(&f.host, f.host.with_file_name("moved")).unwrap();
        symlink(&f.outside, &f.host).unwrap();
        assert_root_changed(lease.verify_root(), "verify");
    }

    #[test]
    fn legacy_overlapping_registry_refused_at_lease() {
        let f = fixture();
        fs::create_dir_all(f.host.join("inner")).unwrap();
        let elsewhere = f.outside.join("tmp-inner");
        fs::create_dir_all(&elsewhere).unwrap();
        f.reg.register_host_dir("inner", &elsewhere).unwrap();
        // Rewrite the registry as an older build would have allowed: `inner` nested in `proj`.
        let path = f.reg.state_root().join(REGISTRY_FILE);
        let text = fs::read_to_string(&path).unwrap();
        let mut db: serde_json::Value = serde_json::from_str(&text).unwrap();
        for w in db["workspaces"].as_array_mut().unwrap() {
            if w["name"] == "inner" {
                w["backing"]["path"] = f.host.join("inner").to_string_lossy().into();
                w.as_object_mut().unwrap().remove("root_identity");
            }
        }
        fs::write(&path, serde_json::to_vec(&db).unwrap()).unwrap();
        let reg = WorkspaceRegistry::open(f.reg.state_root()).unwrap();
        for name in ["proj", "inner"] {
            for mode in [MountMode::ReadWrite, MountMode::ReadOnly] {
                match reg.lease(&name.into(), mode, SandboxId::new()) {
                    Err(SandboxError::Workspace(m)) => {
                        assert!(m.contains("overlaps") && m.contains("workspace rm"), "{m}");
                    }
                    other => panic!("{name}: {other:?}"),
                }
            }
        }
        // Fixing it by removing one makes the other mountable again.
        reg.remove(&"inner".into(), false).unwrap();
        drop(
            reg.lease(&proj(), MountMode::ReadWrite, SandboxId::new())
                .unwrap(),
        );
    }

    #[test]
    fn read_limit_and_streaming() {
        use std::io::Read as _;
        let f = fixture();
        fs::write(f.host.join("big"), vec![1u8; 4096]).unwrap();
        assert!(matches!(
            f.reg.read_file_limited(&proj(), "big", 4095),
            Err(SandboxError::TooLarge(_))
        ));
        assert_eq!(
            f.reg.read_file_limited(&proj(), "big", 4096).unwrap().len(),
            4096
        );
        let (mut file, size) = f.reg.open_file(&proj(), "big").unwrap();
        assert_eq!(size, 4096);
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        assert_eq!(buf.len(), 4096);
        symlink(f.outside.join("secret"), f.host.join("link")).unwrap();
        assert!(matches!(
            f.reg.open_file(&proj(), "link"),
            Err(SandboxError::PathEscape(_))
        ));
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

    /// The reviewer's PoC, in-process: a "guest" thread loops `renameat2(RENAME_EXCHANGE)`
    /// swapping a real directory with a symlink to outside while the host runs file ops.
    #[cfg(target_os = "linux")]
    #[test]
    fn rename_exchange_race_never_escapes() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

        use rustix::fs::{CWD, Mode, OFlags, RenameFlags};

        let f = fixture();
        fs::create_dir_all(f.host.join("real")).unwrap();
        fs::write(f.host.join("real/secret"), b"SAFE").unwrap();
        fs::write(f.outside.join("victim"), b"keep").unwrap();
        symlink(&f.outside, f.host.join("y")).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let swaps = Arc::new(AtomicU64::new(0));
        let guest = {
            let (stop, swaps) = (Arc::clone(&stop), Arc::clone(&swaps));
            let dir = rustix::fs::openat(
                CWD,
                &f.host,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // Errors once the test has failed and its tempdir is gone.
                    if rustix::fs::renameat_with(&dir, "real", &dir, "y", RenameFlags::EXCHANGE)
                        .is_err()
                    {
                        break;
                    }
                    swaps.fetch_add(1, Ordering::Relaxed);
                }
            })
        };

        let (mut ok, mut refused) = (0u32, 0u32);
        for i in 0..2000 {
            match f.reg.read_file(&proj(), "real/secret") {
                Ok(bytes) => {
                    assert_eq!(bytes, b"SAFE", "read escaped at iteration {i}");
                    ok += 1;
                }
                // Symlink refused, or a component swapped mid-walk (not a directory).
                Err(SandboxError::PathEscape(_) | SandboxError::Workspace(_)) => refused += 1,
                Err(e) => panic!("unexpected read error: {e}"),
            }
            let _ = f.reg.put_file(&proj(), "real/pwned", b"PWNED");
            let _ = f.reg.delete_file(&proj(), "real/victim");
            if let Ok(entries) = f.reg.list_files(&proj(), "real") {
                assert!(
                    entries.iter().all(|e| e.path != Path::new("real/victim")),
                    "list escaped at iteration {i}"
                );
            }
            assert!(!f.outside.join("pwned").exists(), "write escaped at {i}");
            assert!(f.outside.join("victim").exists(), "delete escaped at {i}");
        }
        stop.store(true, Ordering::Relaxed);
        guest.join().unwrap();
        assert!(swaps.load(Ordering::Relaxed) > 0);
        assert!(
            refused > 0,
            "race never observed (ok={ok}); test is not exercising it"
        );
        assert_eq!(fs::read(f.outside.join("secret")).unwrap(), b"top secret");
    }

    /// Child half of `leases_are_cross_process`: holds a rw lease until stdin closes.
    #[test]
    fn lease_holder_child() {
        let Some(root) = std::env::var_os("SANDCRAB_TEST_LEASE_ROOT") else {
            return;
        };
        let reg = WorkspaceRegistry::open(root).unwrap();
        let _lease = reg
            .lease(&proj(), MountMode::ReadWrite, SandboxId::new())
            .unwrap();
        println!("LEASE-READY");
        let mut sink = String::new();
        let _ = io::stdin().read_line(&mut sink);
    }

    #[test]
    fn leases_are_cross_process() {
        use std::io::BufRead;
        use std::process::{Command, Stdio};

        let f = fixture();
        fs::write(f.host.join("a.txt"), b"a").unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "registry::tests::lease_holder_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SANDCRAB_TEST_LEASE_ROOT", f.reg.state_root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = io::BufReader::new(child.stdout.take().unwrap()).lines();
        assert!(
            // libtest prints `test <name> ... ` without a newline before the child output.
            lines.any(|l| l.unwrap().contains("LEASE-READY")),
            "child never took the lease"
        );

        // This process has no in-memory lease; only the flock tells it about the child's.
        let busy = |r: Result<()>| matches!(r, Err(SandboxError::WorkspaceBusy(_)));
        assert!(busy(f.reg.put_file(&proj(), "b.txt", b"b")));
        assert!(busy(f.reg.delete_file(&proj(), "a.txt")));
        assert!(busy(f.reg.remove(&proj(), false).map(drop)));
        assert!(busy(
            f.reg
                .lease(&proj(), MountMode::ReadWrite, SandboxId::new())
                .map(drop)
        ));
        assert!(busy(
            f.reg
                .lease(&proj(), MountMode::ReadOnly, SandboxId::new())
                .map(drop)
        ));
        // Reads stay allowed; forced writes go through.
        assert_eq!(f.reg.read_file(&proj(), "a.txt").unwrap(), b"a");
        f.reg.list_files(&proj(), "").unwrap();
        f.reg
            .put_file_with(&proj(), "b.txt", b"b", HostWriteOptions { force: true })
            .unwrap();
        assert_eq!(fs::read(f.host.join("b.txt")).unwrap(), b"b");

        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        f.reg.put_file(&proj(), "c.txt", b"c").unwrap();
        drop(
            f.reg
                .lease(&proj(), MountMode::ReadWrite, SandboxId::new())
                .unwrap(),
        );
    }

    #[test]
    fn ro_lease_in_other_process_blocks_rw_only() {
        let f = fixture();
        let other = WorkspaceRegistry::open(f.reg.state_root()).unwrap();
        let ro = other
            .lease(&proj(), MountMode::ReadOnly, SandboxId::new())
            .unwrap();
        // A second registry instance has its own in-memory state: like another process.
        f.reg.put_file(&proj(), "x", b"x").unwrap();
        let ro2 = f
            .reg
            .lease(&proj(), MountMode::ReadOnly, SandboxId::new())
            .unwrap();
        assert!(matches!(
            f.reg.lease(&proj(), MountMode::ReadWrite, SandboxId::new()),
            Err(SandboxError::WorkspaceBusy(_))
        ));
        drop((ro, ro2));
        let rw = f
            .reg
            .lease(&proj(), MountMode::ReadWrite, SandboxId::new())
            .unwrap();
        assert!(matches!(
            other.put_file(&proj(), "x", b"y"),
            Err(SandboxError::WorkspaceBusy(_))
        ));
        drop(rw);
        other.put_file(&proj(), "x", b"y").unwrap();
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
