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

use std::ffi::OsStr;
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

const DIR_FLAGS: OFlags = OFlags::RDONLY
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

/// Read a regular file. Symlinks, directories and special files are refused.
pub(crate) fn read_file(root: &OwnedFd, rel: &SafeRelPath) -> Result<Vec<u8>> {
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
    let mut buf = Vec::new();
    File::from(fd)
        .read_to_end(&mut buf)
        .map_err(|e| SandboxError::io_at(rel.as_path(), e))?;
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

/// Delete a file, a symlink (the link itself) or a directory tree.
pub(crate) fn delete(root: &OwnedFd, rel: &SafeRelPath) -> Result<()> {
    let (parent, name) = split(rel)?;
    let dir = open_dir(root, rel, &parent, false)?;
    let st = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| map(rel, e))?;
    if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
        remove_tree(&dir, name).map_err(|e| map(rel, e))
    } else {
        rustix::fs::unlinkat(&dir, name, AtFlags::empty()).map_err(|e| map(rel, e))
    }
}

/// Recursively remove directory `name` inside `parent`, fd-relative, never following links.
/// A concurrent swap of a subdirectory for a symlink makes the open fail (`ELOOP`) or, at
/// worst, unlinks the symlink itself; it never reaches outside.
fn remove_tree(parent: &OwnedFd, name: &OsStr) -> std::result::Result<(), Errno> {
    let dir = rustix::fs::openat(parent, name, DIR_FLAGS, Mode::empty())?;
    let mut children = Vec::new();
    for entry in Dir::read_from(&dir)? {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            children.push(OsStr::from_bytes(bytes).to_owned());
        }
    }
    for child in &children {
        match rustix::fs::statat(&dir, child.as_os_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Directory => {
                remove_tree(&dir, child)?;
            }
            Ok(_) => rustix::fs::unlinkat(&dir, child.as_os_str(), AtFlags::empty())?,
            Err(Errno::NOENT) => {}
            Err(e) => return Err(e),
        }
    }
    rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR)
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
            read_file(&root, &rel("fifo")),
            Err(SandboxError::Workspace(_))
        ));
        assert!(matches!(
            read_file(&root, &rel("d")),
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
