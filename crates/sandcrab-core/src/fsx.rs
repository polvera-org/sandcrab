//! Race-free, fd-relative file operations beneath a workspace root.
//!
//! A workspace directory can be mounted read-write into a running sandbox, so its contents
//! are attacker-controlled and can change between any two host syscalls (e.g. a guest
//! looping `renameat2(RENAME_EXCHANGE)` to swap a real directory with a symlink). Checking a
//! path and then re-walking it by name is therefore unsound. Every operation here instead:
//!
//! - opens the workspace root once and walks *fd-relative* from that handle;
//! - never follows a symlink, in any component, including the last one: on Linux the
//!   directory walk is a single `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
//!   RESOLVE_NO_MAGICLINKS)`, elsewhere (or when `openat2` is unavailable) it is a
//!   component-by-component `openat(O_DIRECTORY | O_NOFOLLOW)`;
//! - reads and writes through the opened descriptor (`O_NOFOLLOW`), writes via a fresh
//!   `O_CREAT | O_EXCL` temp file in the target directory fd + `renameat` within that fd,
//!   and deletes via `unlinkat` on the parent directory fd.
//!
//! A symlink anywhere on the path is refused with [`SandboxError::PathEscape`], even when
//! it points inside the workspace: host-side workspace operations do not follow symlinks.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::registry::FileKind;
use crate::{Result, SafeRelPath, SandboxError};

pub(crate) const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Attempts at picking an unused temp-file name before giving up.
const TMP_ATTEMPTS: u32 = 16;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Map a syscall error on `rel` to a [`SandboxError`].
fn map(rel: &SafeRelPath, e: Errno) -> SandboxError {
    match e {
        Errno::LOOP => SandboxError::PathEscape(format!(
            "`{rel}`: path contains a symlink; host-side workspace operations never follow \
             symlinks"
        )),
        Errno::XDEV => SandboxError::PathEscape(format!("`{rel}` resolves outside the workspace")),
        Errno::NOENT => SandboxError::not_found("workspace path", rel),
        Errno::NOTDIR => {
            SandboxError::Workspace(format!("`{rel}`: a path component is not a directory"))
        }
        e => SandboxError::io_at(rel.as_path(), e.into()),
    }
}

fn symlink_refused(rel: &SafeRelPath) -> SandboxError {
    map(rel, Errno::LOOP)
}

/// Plain name components of `rel` (`SafeRelPath` already refused `..` and absolute paths).
fn names(rel: &SafeRelPath) -> Result<Vec<&OsStr>> {
    rel.as_path()
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(Ok(n)),
            Component::CurDir => None,
            _ => Some(Err(SandboxError::PathEscape(format!(
                "`{rel}` is not a plain relative path"
            )))),
        })
        .collect()
}

/// `(parent components, final name)`; the workspace root itself has no final name.
fn split(rel: &SafeRelPath) -> Result<(Vec<&OsStr>, &OsStr)> {
    let mut comps = names(rel)?;
    let name = comps.pop().ok_or_else(|| {
        SandboxError::InvalidConfig("a file path is required, got the workspace root".into())
    })?;
    Ok((comps, name))
}

/// Open a directory by plain path (tests only; production opens roots with
/// [`open_root_nofollow`]).
#[cfg(test)]
pub(crate) fn open_root(path: &Path) -> Result<OwnedFd> {
    rustix::fs::openat(CWD, path, DIR_FLAGS, Mode::empty())
        .map_err(|e| SandboxError::io_at(path, e.into()))
}

/// Why [`open_root_nofollow`] refused a stored workspace root.
#[derive(Debug)]
pub(crate) enum RootOpenError {
    /// A component is a symlink, missing, or not a directory: the root is not what was
    /// registered. Carries a human-readable reason.
    Changed(String),
    Io(io::Error),
}

/// Open the stored (canonical, absolute) workspace root by walking from `/`, one
/// `openat(O_DIRECTORY | O_NOFOLLOW)` per component. Never follows a symlink and never
/// re-resolves the path: a symlink or missing component anywhere is [`RootOpenError::Changed`].
pub(crate) fn open_root_nofollow(path: &Path) -> std::result::Result<OwnedFd, RootOpenError> {
    if !path.is_absolute() {
        return Err(RootOpenError::Changed(format!(
            "`{}` is not an absolute path",
            path.display()
        )));
    }
    let mut cur = rustix::fs::openat(CWD, "/", DIR_FLAGS, Mode::empty())
        .map_err(|e| RootOpenError::Io(e.into()))?;
    let mut walked = PathBuf::from("/");
    for c in path.components() {
        let name = match c {
            Component::RootDir => continue,
            Component::Normal(n) => n,
            _ => {
                return Err(RootOpenError::Changed(format!(
                    "`{}` is not a canonical path",
                    path.display()
                )));
            }
        };
        walked.push(name);
        cur = match rustix::fs::openat(&cur, name, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::LOOP) => {
                return Err(RootOpenError::Changed(format!(
                    "`{}` is now a symlink",
                    walked.display()
                )));
            }
            Err(Errno::NOTDIR) if is_symlink(&cur, name) => {
                return Err(RootOpenError::Changed(format!(
                    "`{}` is now a symlink",
                    walked.display()
                )));
            }
            Err(Errno::NOTDIR) => {
                return Err(RootOpenError::Changed(format!(
                    "`{}` is not a directory",
                    walked.display()
                )));
            }
            Err(Errno::NOENT) => {
                return Err(RootOpenError::Changed(format!(
                    "`{}` does not exist",
                    walked.display()
                )));
            }
            Err(e) => return Err(RootOpenError::Io(io::Error::from(e))),
        };
    }
    Ok(cur)
}

/// `(st_dev, st_ino)` of an open descriptor.
pub(crate) fn identity(fd: &OwnedFd) -> io::Result<(u64, u64)> {
    let st = rustix::fs::fstat(fd)?;
    #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
    Ok((st.st_dev as u64, st.st_ino as u64))
}

/// Open the directory `comps` beneath `root` without following any symlink, optionally
/// creating missing directories along the way.
fn open_dir(root: &OwnedFd, rel: &SafeRelPath, comps: &[&OsStr], create: bool) -> Result<OwnedFd> {
    if comps.is_empty() {
        return root
            .try_clone()
            .map_err(|e| SandboxError::io_at(rel.as_path(), e));
    }
    #[cfg(target_os = "linux")]
    if !create {
        use rustix::fs::ResolveFlags;
        let path: PathBuf = comps.iter().collect();
        match rustix::fs::openat2(
            root,
            &path,
            DIR_FLAGS,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        ) {
            Ok(fd) => return Ok(fd),
            // No openat2 (old kernel, seccomp), a spurious resolution retry, or ENOTDIR
            // (possibly a symlink as the last component): walk instead, which classifies.
            Err(Errno::NOSYS | Errno::PERM | Errno::AGAIN | Errno::NOTDIR) => {}
            Err(e) => return Err(map(rel, e)),
        }
    }
    walk(root, rel, comps, create)
}

fn is_symlink(dir: impl AsFd, name: &OsStr) -> bool {
    rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Symlink)
}

/// Portable fallback: one `openat(O_DIRECTORY | O_NOFOLLOW)` per component.
fn walk(root: &OwnedFd, rel: &SafeRelPath, comps: &[&OsStr], create: bool) -> Result<OwnedFd> {
    let mut cur: Option<OwnedFd> = None;
    for name in comps {
        let dir = cur.as_ref().map_or(root.as_fd(), AsFd::as_fd);
        if create {
            match rustix::fs::mkdirat(dir, *name, Mode::from_raw_mode(0o777)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(map(rel, e)),
            }
        }
        let next = match rustix::fs::openat(dir, *name, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            // Linux reports `O_DIRECTORY | O_NOFOLLOW` on a symlink as ENOTDIR.
            Err(Errno::NOTDIR) if is_symlink(dir, name) => return Err(symlink_refused(rel)),
            Err(e) => return Err(map(rel, e)),
        };
        cur = Some(next);
    }
    match cur {
        Some(fd) => Ok(fd),
        None => root
            .try_clone()
            .map_err(|e| SandboxError::io_at(rel.as_path(), e)),
    }
}

/// Open a regular file for reading: `(file, size)`. Symlinks, directories and special
/// files are refused.
pub(crate) fn open_file(root: &OwnedFd, rel: &SafeRelPath) -> Result<(File, u64)> {
    let (parent, name) = split(rel)?;
    let dir = open_dir(root, rel, &parent, false)?;
    // O_NONBLOCK: a guest-planted FIFO must not hang the host; refused right below.
    let fd = rustix::fs::openat(
        &dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY,
        Mode::empty(),
    )
    .map_err(|e| map(rel, e))?;
    let st = rustix::fs::fstat(&fd).map_err(|e| map(rel, e))?;
    match FileType::from_raw_mode(st.st_mode) {
        FileType::RegularFile => {}
        FileType::Directory => {
            return Err(SandboxError::Workspace(format!("`{rel}` is a directory")));
        }
        _ => {
            return Err(SandboxError::Workspace(format!(
                "`{rel}` is not a regular file"
            )));
        }
    }
    Ok((File::from(fd), u64::try_from(st.st_size).unwrap_or(0)))
}

fn too_large(rel: &SafeRelPath, limit: u64) -> SandboxError {
    SandboxError::TooLarge(format!(
        "`{rel}` is larger than the {limit}-byte read limit; stream it instead"
    ))
}

/// Read a regular file of at most `limit` bytes ([`SandboxError::TooLarge`] beyond, also
/// when it grows past the limit while being read).
pub(crate) fn read_file(root: &OwnedFd, rel: &SafeRelPath, limit: u64) -> Result<Vec<u8>> {
    let (file, size) = open_file(root, rel)?;
    if size > limit {
        return Err(too_large(rel, limit));
    }
    let mut buf = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    file.take(limit.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| SandboxError::io_at(rel.as_path(), e))?;
    if buf.len() as u64 > limit {
        return Err(too_large(rel, limit));
    }
    Ok(buf)
}

/// Atomically write `bytes` to `rel`, creating parent directories. An existing regular
/// file's permission bits are kept. Replacing a symlink or a directory is refused.
pub(crate) fn write_file(root: &OwnedFd, rel: &SafeRelPath, bytes: &[u8]) -> Result<()> {
    let (parent, name) = split(rel)?;
    let dir = open_dir(root, rel, &parent, true)?;
    // Advisory pre-check for a clear error; the rename below never follows a link anyway.
    let keep_mode = match rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => match FileType::from_raw_mode(st.st_mode) {
            FileType::RegularFile => Some(Mode::from_raw_mode(st.st_mode & 0o7777)),
            FileType::Symlink => return Err(symlink_refused(rel)),
            FileType::Directory => {
                return Err(SandboxError::Workspace(format!("`{rel}` is a directory")));
            }
            _ => {
                return Err(SandboxError::Workspace(format!(
                    "`{rel}` exists and is not a regular file"
                )));
            }
        },
        Err(Errno::NOENT) => None,
        Err(e) => return Err(map(rel, e)),
    };

    let (tmp, fd) = create_tmp(&dir, rel)?;
    let write = || -> io::Result<()> {
        if let Some(mode) = keep_mode {
            rustix::fs::fchmod(&fd, mode)?;
        }
        let mut file = File::from(fd.try_clone()?);
        file.write_all(bytes)?;
        file.sync_all()?;
        rustix::fs::renameat(&dir, &tmp, &dir, name)?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = rustix::fs::unlinkat(&dir, &tmp, AtFlags::empty());
        return Err(SandboxError::io_at(rel.as_path(), e));
    }
    // Persist the rename; best effort on filesystems that refuse directory fsync.
    let _ = rustix::fs::fsync(&dir);
    Ok(())
}

/// Create a fresh, never-before-existing temp file in `dir` (`O_EXCL | O_NOFOLLOW`).
fn create_tmp(dir: &OwnedFd, rel: &SafeRelPath) -> Result<(PathBuf, OwnedFd)> {
    for _ in 0..TMP_ATTEMPTS {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let tmp = PathBuf::from(format!(".sandcrab-put.{}.{nanos}.{n}", std::process::id()));
        match rustix::fs::openat(
            dir,
            &tmp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o666),
        ) {
            Ok(fd) => return Ok((tmp, fd)),
            Err(Errno::EXIST) => {}
            Err(e) => return Err(map(rel, e)),
        }
    }
    Err(SandboxError::Workspace(format!(
        "`{rel}`: could not create a temporary file next to it"
    )))
}

/// Deepest directory nesting [`delete`] removes; deeper trees are refused.
pub(crate) const MAX_REMOVE_DEPTH: usize = 4096;
/// Directory descriptors [`remove_tree`] keeps open at once; ancestors beyond the window are
/// closed and re-opened (fd-relative, no symlinks) when the walk returns to them.
const REMOVE_FD_WINDOW: usize = 64;

/// Delete a file, a symlink (the link itself) or a directory tree.
pub(crate) fn delete(root: &OwnedFd, rel: &SafeRelPath) -> Result<()> {
    delete_max_depth(root, rel, MAX_REMOVE_DEPTH)
}

fn delete_max_depth(root: &OwnedFd, rel: &SafeRelPath, max_depth: usize) -> Result<()> {
    let (parent, name) = split(rel)?;
    let dir = open_dir(root, rel, &parent, false)?;
    let st = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| map(rel, e))?;
    if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
        remove_tree(&dir, name, rel, max_depth)
    } else {
        rustix::fs::unlinkat(&dir, name, AtFlags::empty()).map_err(|e| map(rel, e))
    }
}

/// One directory being emptied by [`remove_tree`].
struct Frame {
    /// Name inside the previous frame's directory (or the base parent for frame 0).
    name: OsString,
    /// Open handle, or `None` while closed to bound descriptor use.
    fd: Option<OwnedFd>,
    /// Entries not yet removed.
    pending: Vec<OsString>,
}

fn read_children(dir: &OwnedFd) -> std::result::Result<Vec<OsString>, Errno> {
    let mut out = Vec::new();
    for entry in Dir::read_from(dir)? {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            out.push(OsStr::from_bytes(bytes).to_owned());
        }
    }
    Ok(out)
}

/// Make sure `stack[i]` has an open descriptor, re-opening it from the nearest open
/// ancestor (or `base`) by name, `O_NOFOLLOW` at every step.
fn reopen(base: &OwnedFd, stack: &mut [Frame], i: usize) -> std::result::Result<(), Errno> {
    if stack[i].fd.is_some() {
        return Ok(());
    }
    let start = (0..i).rev().find(|&j| stack[j].fd.is_some());
    let mut cur: Option<OwnedFd> = None;
    for k in start.map_or(0, |j| j + 1)..=i {
        let dir = match (&cur, start) {
            (Some(c), _) => c.as_fd(),
            (None, Some(j)) => stack[j].fd.as_ref().map_or(base.as_fd(), AsFd::as_fd),
            (None, None) => base.as_fd(),
        };
        cur = Some(rustix::fs::openat(
            dir,
            stack[k].name.as_os_str(),
            DIR_FLAGS,
            Mode::empty(),
        )?);
    }
    stack[i].fd = cur;
    Ok(())
}

/// Remove directory `name` inside `parent`, fd-relative, never following links. Iterative
/// (explicit stack), refuses trees deeper than `max_depth` ([`MAX_REMOVE_DEPTH`]), and keeps at most
/// [`REMOVE_FD_WINDOW`] directory descriptors open. A concurrent swap of a subdirectory for
/// a symlink makes the open fail (`ELOOP`/`ENOTDIR`) or, at worst, unlinks the symlink
/// itself; it never reaches outside.
fn remove_tree(parent: &OwnedFd, name: &OsStr, rel: &SafeRelPath, max_depth: usize) -> Result<()> {
    let err = |e| map(rel, e);
    let fd = rustix::fs::openat(parent, name, DIR_FLAGS, Mode::empty()).map_err(err)?;
    let pending = read_children(&fd).map_err(err)?;
    let mut stack = vec![Frame {
        name: name.to_owned(),
        fd: Some(fd),
        pending,
    }];
    while let Some(top) = stack.len().checked_sub(1) {
        let Some(child) = stack[top].pending.pop() else {
            let done = stack.pop().expect("non-empty stack");
            drop(done.fd);
            let dir = match stack.len().checked_sub(1) {
                Some(p) => {
                    reopen(parent, &mut stack, p).map_err(err)?;
                    stack[p].fd.as_ref().map_or(parent.as_fd(), AsFd::as_fd)
                }
                None => parent.as_fd(),
            };
            match rustix::fs::unlinkat(dir, done.name.as_os_str(), AtFlags::REMOVEDIR) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(e) => return Err(err(e)),
            }
            continue;
        };
        reopen(parent, &mut stack, top).map_err(err)?;
        let Some(dir) = stack[top].fd.as_ref() else {
            unreachable!("reopen leaves the frame open")
        };
        match rustix::fs::statat(dir, child.as_os_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Directory => {
                if stack.len() >= max_depth {
                    return Err(SandboxError::Workspace(format!(
                        "`{rel}`: directory tree is deeper than {max_depth} levels; \
                         refusing to delete it"
                    )));
                }
                let fd = match rustix::fs::openat(dir, child.as_os_str(), DIR_FLAGS, Mode::empty())
                {
                    Ok(fd) => fd,
                    Err(Errno::NOENT) => continue,
                    Err(e) => return Err(err(e)),
                };
                let pending = read_children(&fd).map_err(err)?;
                stack.push(Frame {
                    name: child,
                    fd: Some(fd),
                    pending,
                });
                if let Some(old) = stack.len().checked_sub(REMOVE_FD_WINDOW + 1) {
                    stack[old].fd = None;
                }
            }
            Ok(_) => match rustix::fs::unlinkat(dir, child.as_os_str(), AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(e) => return Err(err(e)),
            },
            Err(Errno::NOENT) => {}
            Err(e) => return Err(err(e)),
        }
    }
    Ok(())
}

/// List one directory: `(name, kind, size)`, symlinks reported (`lstat`), not followed.
pub(crate) fn list_dir(root: &OwnedFd, rel: &SafeRelPath) -> Result<Vec<(PathBuf, FileKind, u64)>> {
    let comps = names(rel)?;
    let dir = open_dir(root, rel, &comps, false)?;
    let mut out = Vec::new();
    for entry in Dir::read_from(&dir).map_err(|e| map(rel, e))? {
        let entry = entry.map_err(|e| map(rel, e))?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let st = match rustix::fs::statat(&dir, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            // Removed between readdir and stat.
            Err(Errno::NOENT) => continue,
            Err(e) => return Err(map(rel, e)),
        };
        let kind = match FileType::from_raw_mode(st.st_mode) {
            FileType::Symlink => FileKind::Symlink,
            FileType::Directory => FileKind::Dir,
            FileType::RegularFile => FileKind::File,
            _ => FileKind::Other,
        };
        let size = u64::try_from(st.st_size).unwrap_or(0);
        out.push((PathBuf::from(OsStr::from_bytes(bytes)), kind, size));
    }
    Ok(out)
}

/// A fresh directory made beneath a host root that other users may write (a mount root),
/// and used only through descriptors: the directory, its subdirectories and its files are
/// created, written and chowned relative to the handle opened when it was made, never by
/// path. Someone who renames it away and plants a symlink or another directory at its name
/// cannot redirect a write or a chown. Removed on drop: its contents through the held
/// handle, then the (by then empty) name.
pub struct ScratchDir {
    parent: OwnedFd,
    name: OsString,
    path: PathBuf,
    dir: OwnedFd,
}

impl ScratchDir {
    /// Make `root/name` (`name` one plain component, which must not exist yet). `root` is
    /// an absolute, canonical path; a symlink anywhere in it is refused.
    pub fn create(root: &Path, name: &str) -> Result<Self> {
        let rel = SafeRelPath::new(name)?;
        let (parent_comps, n) = split(&rel)?;
        if !parent_comps.is_empty() {
            return Err(SandboxError::InvalidConfig(format!(
                "`{name}` is not a single name"
            )));
        }
        let parent = open_root_nofollow(root).map_err(|e| match e {
            RootOpenError::Changed(m) => SandboxError::PathEscape(m),
            RootOpenError::Io(e) => SandboxError::io_at(root, e),
        })?;
        rustix::fs::mkdirat(&parent, n, Mode::from_raw_mode(0o755)).map_err(|e| map(&rel, e))?;
        let dir = rustix::fs::openat(&parent, n, DIR_FLAGS, Mode::empty());
        let dir = match dir {
            Ok(fd) => fd,
            Err(e) => {
                let _ = rustix::fs::unlinkat(&parent, n, AtFlags::REMOVEDIR);
                return Err(map(&rel, e));
            }
        };
        Ok(Self {
            parent,
            name: n.to_owned(),
            path: root.join(n),
            dir,
        })
    }

    /// The path it was made at (for mounts and messages; never used to reach it).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The handle of `name` inside, never through a symlink.
    fn open_sub(&self, name: &str) -> Result<OwnedFd> {
        let rel = SafeRelPath::new(name)?;
        let comps = names(&rel)?;
        open_dir(&self.dir, &rel, &comps, false)
    }

    /// Make the subdirectory `name` (one plain component).
    pub fn mkdir(&self, name: &str) -> Result<()> {
        let rel = SafeRelPath::new(name)?;
        rustix::fs::mkdirat(&self.dir, rel.as_path(), Mode::from_raw_mode(0o755))
            .map_err(|e| map(&rel, e))
    }

    /// Create the new file `rel` (`O_CREAT | O_EXCL | O_NOFOLLOW`) holding `bytes`.
    pub fn write_new(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        let rel = SafeRelPath::new(rel)?;
        let (parent, name) = split(&rel)?;
        let dir = open_dir(&self.dir, &rel, &parent, false)?;
        let fd = rustix::fs::openat(
            &dir,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )
        .map_err(|e| map(&rel, e))?;
        // Readable whatever the umask is: a sandbox user reads it through a mount.
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o644)).map_err(|e| map(&rel, e))?;
        File::from(fd)
            .write_all(bytes)
            .map_err(|e| SandboxError::io_at(rel.as_path(), e))
    }

    /// Give the subdirectory `name` to `uid:gid` (`fchown` on its handle), unless it is
    /// theirs already.
    pub fn chown_dir(&self, name: &str, uid: u32, gid: u32) -> Result<()> {
        let fd = self.open_sub(name)?;
        let err = |e: Errno| SandboxError::io_at(&self.path.join(name), e.into());
        let st = rustix::fs::fstat(&fd).map_err(err)?;
        if st.st_uid == uid && st.st_gid == gid {
            return Ok(());
        }
        rustix::fs::fchown(
            &fd,
            Some(rustix::process::Uid::from_raw(uid)),
            Some(rustix::process::Gid::from_raw(gid)),
        )
        .map_err(err)
    }

    /// Read the regular file `rel` inside, at most `limit` bytes; a symlink is refused.
    pub fn read(&self, rel: &str, limit: u64) -> Result<Vec<u8>> {
        read_file(&self.dir, &SafeRelPath::new(rel)?, limit)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let removed = read_children(&self.dir)
            .map_err(|e| SandboxError::io_at(&self.path, e.into()))
            .and_then(|children| {
                for child in children {
                    let rel = SafeRelPath::new(&child)?;
                    delete(&self.dir, &rel)?;
                }
                Ok(())
            });
        // Empty now, unless someone else put something there; `rmdir` removes only an empty
        // directory, so a directory planted at the name is never emptied.
        let removed = removed.and_then(|()| {
            match rustix::fs::unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR) {
                Ok(()) | Err(Errno::NOENT) => Ok(()),
                Err(e) => Err(SandboxError::io_at(&self.path, e.into())),
            }
        });
        if let Err(e) = removed {
            tracing::warn!(dir = %self.path.display(), error = %e, "cannot remove scratch directory");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    fn rel(s: &str) -> SafeRelPath {
        SafeRelPath::new(s).unwrap()
    }

    #[test]
    fn walk_refuses_symlink_components() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(ws.join("real/sub")).unwrap();
        symlink("real", ws.join("alias")).unwrap();
        symlink(tmp.path(), ws.join("up")).unwrap();
        let root = open_root(&ws).unwrap();
        for create in [false, true] {
            walk(
                &root,
                &rel("real/sub"),
                &names(&rel("real/sub")).unwrap(),
                create,
            )
            .unwrap();
            for bad in ["alias", "alias/sub", "up", "up/ws"] {
                let r = rel(bad);
                assert!(
                    matches!(
                        walk(&root, &r, &names(&r).unwrap(), create),
                        Err(SandboxError::PathEscape(_))
                    ),
                    "{bad} create={create}"
                );
            }
        }
        // `create` never materializes anything through a symlink.
        assert!(!tmp.path().join("ws/x").exists());
    }

    #[test]
    fn read_refuses_fifo_and_dir() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("d")).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(tmp.path().join("fifo"))
            .status()
            .unwrap();
        assert!(status.success());
        let root = open_root(tmp.path()).unwrap();
        assert!(matches!(
            read_file(&root, &rel("fifo"), u64::MAX),
            Err(SandboxError::Workspace(_))
        ));
        assert!(matches!(
            read_file(&root, &rel("d"), u64::MAX),
            Err(SandboxError::Workspace(_))
        ));
    }

    #[test]
    fn write_keeps_mode_and_leaves_no_tmp() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("run.sh");
        fs::write(&f, b"old").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o750)).unwrap();
        let root = open_root(tmp.path()).unwrap();
        write_file(&root, &rel("run.sh"), b"new").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o7777,
            0o750
        );
        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["run.sh"]);
    }

    #[test]
    fn read_cap_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("big"), vec![7u8; 1000]).unwrap();
        let root = open_root(tmp.path()).unwrap();
        assert_eq!(read_file(&root, &rel("big"), 1000).unwrap().len(), 1000);
        assert!(matches!(
            read_file(&root, &rel("big"), 999),
            Err(SandboxError::TooLarge(_))
        ));
    }

    #[test]
    fn deep_tree_removed_iteratively_with_bounded_fds() {
        let tmp = tempfile::tempdir().unwrap();
        // Deeper than the fd window (and than a default 1024-fd ulimit would allow if every
        // level kept its descriptor), built fd-relative to dodge PATH_MAX.
        let depth = 1500;
        let root = open_root(tmp.path()).unwrap();
        let mut cur = root.try_clone().unwrap();
        for i in 0..depth {
            rustix::fs::mkdirat(&cur, "d", Mode::from_raw_mode(0o755)).unwrap();
            if i % 100 == 0 {
                let f = rustix::fs::openat(
                    &cur,
                    "f",
                    OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC,
                    Mode::from_raw_mode(0o644),
                )
                .unwrap();
                drop(f);
            }
            cur = rustix::fs::openat(&cur, "d", DIR_FLAGS, Mode::empty()).unwrap();
        }
        drop(cur);
        delete(&root, &rel("d")).unwrap();
        assert!(!tmp.path().join("d").exists());
    }

    #[test]
    fn too_deep_tree_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = open_root(tmp.path()).unwrap();
        let mut cur = root.try_clone().unwrap();
        for _ in 0..10 {
            rustix::fs::mkdirat(&cur, "d", Mode::from_raw_mode(0o755)).unwrap();
            cur = rustix::fs::openat(&cur, "d", DIR_FLAGS, Mode::empty()).unwrap();
        }
        drop(cur);
        assert!(matches!(
            delete_max_depth(&root, &rel("d"), 5),
            Err(SandboxError::Workspace(m)) if m.contains("deeper")
        ));
        delete_max_depth(&root, &rel("d"), 10).unwrap();
        assert!(!tmp.path().join("d").exists());
    }

    /// B1: once made, a scratch directory renamed away and replaced by a symlink (or by
    /// another directory) never takes a write, a chown or a removal outside it.
    #[test]
    fn scratch_dir_survives_a_swap_at_its_name() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), "x").unwrap();

        let s = ScratchDir::create(&root, "scratch").unwrap();
        assert!(ScratchDir::create(&root, "scratch").is_err(), "must be new");
        assert!(ScratchDir::create(&root, "a/b").is_err());
        // Swap: the real directory moves away, a symlink to `outside` takes its name.
        fs::rename(root.join("scratch"), root.join("moved")).unwrap();
        symlink(&outside, root.join("scratch")).unwrap();

        s.mkdir("rw").unwrap();
        s.write_new("canary-outside", b"c").unwrap();
        s.write_new("rw/f", b"c").unwrap();
        assert!(s.write_new("rw/f", b"again").is_err(), "O_EXCL");
        let (uid, gid) = (
            crate::private_dir::current_uid(),
            crate::private_dir::current_gid(),
        );
        s.chown_dir("rw", uid, gid).unwrap();
        assert_eq!(s.read("rw/f", 16).unwrap(), b"c");
        // A symlink planted inside is not followed by a read.
        symlink(outside.join("keep"), root.join("moved/rw/link")).unwrap();
        assert!(s.read("rw/link", 16).is_err());

        assert!(root.join("moved/rw/f").exists());
        assert!(root.join("moved/canary-outside").exists());
        let names: Vec<_> = fs::read_dir(&outside)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["keep"], "nothing written outside");
        assert_eq!(fs::metadata(&outside).unwrap().uid(), uid);

        drop(s);
        // The held directory is emptied; the planted symlink and its target are untouched.
        assert_eq!(fs::read_dir(root.join("moved")).unwrap().count(), 0);
        assert!(
            fs::symlink_metadata(root.join("scratch"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "x");
    }

    #[test]
    fn scratch_dir_is_removed_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let s = ScratchDir::create(&base, "s").unwrap();
        s.mkdir("ro").unwrap();
        s.write_new("ro/canary", b"c").unwrap();
        assert_eq!(s.path(), base.join("s"));
        assert_eq!(fs::read(base.join("s/ro/canary")).unwrap(), b"c");
        drop(s);
        assert!(!base.join("s").exists());
    }

    #[test]
    fn root_open_refuses_symlink_components() {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir_all(base.join("real/ws")).unwrap();
        symlink(base.join("real"), base.join("link")).unwrap();
        assert!(open_root_nofollow(&base.join("real/ws")).is_ok());
        for bad in ["link/ws", "link", "missing", "real/../real/ws"] {
            assert!(
                matches!(
                    open_root_nofollow(&base.join(bad)),
                    Err(RootOpenError::Changed(_))
                ),
                "{bad}"
            );
        }
    }
}
