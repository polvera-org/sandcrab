//! Private state directories (reviews R14, R18).
//!
//! Whoever can write a directory can replace any file in it, or rename a subdirectory away and
//! put their own in its place. The Sandcrab state tree holds pinned binaries next to the records
//! that pin them (the libkrun helper and its `.pin.json`, libraries and netstack next to their
//! manifests, rootfs images next to theirs). So a group member who can write the tree could
//! rewrite a file *and* its record. New directories are created `0700` whatever the umask, but
//! a tree created earlier under a `0002` umask is `0775`.
//!
//! Policy for a directory **in the state tree** (the state root and every directory from it down
//! to the ones a backend uses):
//!
//! - a symlink as the state root, something that is not a directory, or a directory owned by
//!   another uid is **refused** (the error says how to fix it);
//! - a directory owned by this user with loose permission bits is **tightened to `0700`** and
//!   logged at `warn`. Tightening never breaks an install that only this user uses, and doing it
//!   at backend construction closes the window for every later spawn. The warning also says to
//!   reinstall if group members are not trusted, since what they could write before is not
//!   re-proven by tightening.
//!
//! A directory **outside** the state tree (an explicit override such as a custom `lib_dir`) is
//! not changed: it must be owned by this user or root and not group/world-writable, else it is
//! refused.
//!
//! Each check opens the directory (`O_NOFOLLOW` where symlinks are refused), `fstat`s and
//! `fchmod`s that fd, so the inode checked is the inode changed. Parents are handled before
//! children, so once a parent is private nobody else can swap the child under it.

use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

use crate::{Result, SandboxError};

/// Group- and world-write: the bits that let others replace files (the state tree).
pub const GROUP_WORLD_WRITE: u32 = 0o022;
/// Every group and world bit (directories that hold secrets, e.g. Docker's auth tokens).
pub const GROUP_WORLD_ANY: u32 = 0o077;

/// This process's real uid.
#[must_use]
pub fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// A directory [`secure_dir`] or [`secure_state_tree`] tightened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tightened {
    pub path: PathBuf,
    /// Permission bits before (`& 0o7777`).
    pub was: u32,
}

/// Make one directory private: refuse a symlink (unless `follow`), a non-directory or another
/// owner; chmod it to `0700` when any of the `loose` bits is set. `purpose` completes the error
/// ("… must be a private directory owned by this user; <purpose>"). Returns the old mode when it
/// tightened.
pub fn secure_dir(
    dir: &Path,
    uid: u32,
    loose: u32,
    follow: bool,
    purpose: &str,
) -> Result<Option<Tightened>> {
    let refuse = |why: String| {
        SandboxError::InvalidConfig(format!(
            "`{}` {why}; it must be a private directory owned by this user (uid {uid}): \
             {purpose}",
            dir.display()
        ))
    };
    if !follow
        && std::fs::symlink_metadata(dir)
            .map_err(|e| SandboxError::io_at(dir, e))?
            .file_type()
            .is_symlink()
    {
        return Err(refuse(
            "is a symlink (point the configuration at the real directory)".into(),
        ));
    }
    let fd = open_dir(dir, follow).map_err(|e| match e {
        Errno::NOTDIR => refuse("is not a directory".into()),
        Errno::LOOP => refuse("is a symlink".into()),
        e => SandboxError::io_at(dir, e.into()),
    })?;
    let st = rustix::fs::fstat(&fd).map_err(|e| SandboxError::io_at(dir, e.into()))?;
    if st.st_uid != uid {
        return Err(refuse(format!(
            "is owned by uid {}, not {uid} (`chown -R {uid} {}`, or use another state dir)",
            st.st_uid,
            dir.display()
        )));
    }
    let mode = perm_bits(&st);
    if mode & loose == 0 {
        return Ok(None);
    }
    rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700))
        .map_err(|e| SandboxError::io_at(dir, e.into()))?;
    Ok(Some(Tightened {
        path: dir.to_path_buf(),
        was: mode,
    }))
}

/// Permission bits of a stat (`st_mode` is `u16` on macOS, `u32` on Linux).
#[allow(clippy::useless_conversion)]
fn perm_bits(st: &rustix::fs::Stat) -> u32 {
    u32::from(st.st_mode) & 0o7777
}

fn open_dir(dir: &Path, follow: bool) -> rustix::io::Result<OwnedFd> {
    let mut flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    if !follow {
        flags |= OFlags::NOFOLLOW;
    }
    rustix::fs::open(dir, flags, Mode::empty())
}

/// Check a directory outside the state tree without changing it: owned by `uid` or root and not
/// group/world-writable (symlinks are followed: the target is what counts).
pub fn check_foreign_dir(dir: &Path, uid: u32, purpose: &str) -> Result<()> {
    let fd = open_dir(dir, true).map_err(|e| match e {
        Errno::NOTDIR => SandboxError::InvalidConfig(format!(
            "`{}` is not a directory ({purpose})",
            dir.display()
        )),
        e => SandboxError::io_at(dir, e.into()),
    })?;
    let st = rustix::fs::fstat(&fd).map_err(|e| SandboxError::io_at(dir, e.into()))?;
    let mode = perm_bits(&st);
    if (st.st_uid != uid && st.st_uid != 0) || mode & GROUP_WORLD_WRITE != 0 {
        return Err(SandboxError::InvalidConfig(format!(
            "`{}` (uid {}, mode {mode:o}) could be written by other users: it must be owned \
             by uid {uid} or root and not group/world-writable (`chmod go-w {}`); {purpose}",
            dir.display(),
            st.st_uid,
            dir.display()
        )));
    }
    Ok(())
}

/// Secure the state tree of a backend: `root` itself (never a symlink) and every existing
/// directory from `root` down to each of `dirs` that lies under it ([`secure_dir`] with
/// [`GROUP_WORLD_WRITE`]; a symlink *below* the root is followed and its target checked the same
/// way). A directory of `dirs` outside `root` gets [`check_foreign_dir`]. Missing directories are
/// skipped (they are created `0700` later). Every tightened directory is logged at `warn`.
pub fn secure_state_tree(
    backend: &str,
    root: &Path,
    dirs: &[&Path],
    uid: u32,
) -> Result<Vec<Tightened>> {
    const PURPOSE: &str = "whoever can write the Sandcrab state tree can replace pinned \
                           binaries together with the records that pin them";
    let mut done: Vec<PathBuf> = Vec::new();
    let mut tightened = Vec::new();
    let mut visit = |p: &Path, follow: bool, out: &mut Vec<Tightened>| -> Result<bool> {
        if done.iter().any(|d| d == p) {
            return Ok(true);
        }
        if std::fs::symlink_metadata(p).is_err() {
            return Ok(false); // missing: nothing below it exists either
        }
        if let Some(t) = secure_dir(p, uid, GROUP_WORLD_WRITE, follow, PURPOSE)? {
            out.push(t);
        }
        done.push(p.to_path_buf());
        Ok(true)
    };
    if !visit(root, false, &mut tightened)? {
        return Ok(tightened);
    }
    for dir in dirs {
        match under(root, dir) {
            Some(rel) => {
                let mut p = root.to_path_buf();
                for c in rel {
                    p.push(c);
                    if !visit(&p, true, &mut tightened)? {
                        break;
                    }
                }
            }
            None => {
                if dir.exists() {
                    check_foreign_dir(dir, uid, PURPOSE)?;
                }
            }
        }
    }
    for t in &tightened {
        tracing::warn!(
            backend,
            dir = %t.path.display(),
            was = format!("{:o}", t.was),
            "state directory was writable by other users; tightened it to 0700 (if you do not \
             trust every member of its group, reinstall the pinned artifacts)"
        );
    }
    Ok(tightened)
}

/// The normal components of `dir` below `root` (lexically); `None` when `dir` is not under
/// `root` or the remainder has `..`/`.` components.
fn under<'a>(root: &Path, dir: &'a Path) -> Option<Vec<&'a std::ffi::OsStr>> {
    let rel = dir.strip_prefix(root).ok()?;
    rel.components()
        .map(|c| match c {
            Component::Normal(n) => Some(n),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn mkdir(p: &Path, m: u32) {
        fs::create_dir_all(p).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
    }

    fn uid() -> u32 {
        rustix::process::getuid().as_raw()
    }

    #[test]
    fn group_writable_tree_is_tightened_down_to_each_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home");
        mkdir(&root, 0o775);
        mkdir(&root.join("bin"), 0o775);
        mkdir(&root.join("artifacts/libkrun/1.0"), 0o775);
        mkdir(&root.join("artifacts/other"), 0o777);
        mkdir(&root.join("cache"), 0o755);
        let bin = root.join("bin");
        let lib = root.join("artifacts/libkrun/1.0");
        let missing = root.join("run/krun");
        let cache = root.join("cache");
        let t = secure_state_tree("test", &root, &[&bin, &lib, &missing, &cache], uid()).unwrap();
        let paths: Vec<_> = t.iter().map(|t| t.path.clone()).collect();
        assert_eq!(
            paths,
            [
                root.clone(),
                bin.clone(),
                root.join("artifacts"),
                root.join("artifacts/libkrun"),
                lib.clone()
            ]
        );
        assert!(t.iter().all(|t| t.was == 0o775));
        for p in [&root, &bin, &lib] {
            assert_eq!(mode(p), 0o700, "{}", p.display());
        }
        assert_eq!(mode(&cache), 0o755, "not writable by others: left alone");
        assert_eq!(mode(&root.join("artifacts/other")), 0o777, "not on a path");
        // Idempotent.
        assert!(
            secure_state_tree("test", &root, &[&bin, &lib], uid())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn symlinked_root_and_foreign_owner_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        mkdir(&real, 0o700);
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let e = secure_state_tree("test", &link, &[], uid()).unwrap_err();
        assert!(e.to_string().contains("symlink"), "{e}");
        let e = secure_state_tree("test", &real, &[], uid() + 1).unwrap_err();
        assert!(e.to_string().contains("owned by uid"), "{e}");
        let file = tmp.path().join("file");
        fs::write(&file, b"").unwrap();
        let e = secure_state_tree("test", &file, &[], uid()).unwrap_err();
        assert!(e.to_string().contains("not a directory"), "{e}");
    }

    #[test]
    fn symlink_below_the_root_is_followed_and_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home");
        mkdir(&root, 0o700);
        let elsewhere = tmp.path().join("bigdisk-cache");
        mkdir(&elsewhere, 0o775);
        std::os::unix::fs::symlink(&elsewhere, root.join("cache")).unwrap();
        let cache = root.join("cache");
        let t = secure_state_tree("test", &root, &[&cache], uid()).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(mode(&elsewhere), 0o700);
    }

    #[test]
    fn dirs_outside_the_root_are_checked_not_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home");
        mkdir(&root, 0o700);
        let lib = tmp.path().join("custom-lib");
        mkdir(&lib, 0o755);
        secure_state_tree("test", &root, &[&lib], uid()).unwrap();
        fs::set_permissions(&lib, fs::Permissions::from_mode(0o775)).unwrap();
        let e = secure_state_tree("test", &root, &[&lib], uid()).unwrap_err();
        assert!(e.to_string().contains("chmod go-w"), "{e}");
        assert_eq!(mode(&lib), 0o775, "never chmodded outside the tree");
        // `..` is not "under" the root.
        let sneaky = root.join("../custom-lib");
        assert!(secure_state_tree("test", &root, &[&sneaky], uid()).is_err());
        // Root-owned and not writable by others is fine.
        secure_state_tree("test", &root, &[Path::new("/")], uid()).unwrap();
    }

    #[test]
    fn missing_root_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("nope");
        let bin = root.join("bin");
        assert!(
            secure_state_tree("test", &root, &[&bin], uid())
                .unwrap()
                .is_empty()
        );
    }
}
