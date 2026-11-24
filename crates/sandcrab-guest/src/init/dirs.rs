//! Symlink-safe creation of workspace mount points and `default_cwd`.
//!
//! Mount points and `default_cwd` can live inside an earlier, writable workspace, and a
//! workspace persists across turns: a previous turn can have planted
//! `/workspace/data -> /etc`. `crabd init` runs as root, so it must never follow a symlink
//! while creating or mounting such a path. [`ensure_dir_in`] walks one component at a time
//! with `O_NOFOLLOW | O_DIRECTORY`, creates what is missing with `mkdirat`, and returns an
//! fd for the leaf; mounting goes through that fd (`/proc/self/fd/N`), so the path is never
//! re-resolved by name.
//!
//! Portable (unit-tested on every Unix); only the mount itself is Linux-only.

use std::{
    io,
    os::fd::{AsFd, OwnedFd},
    path::{Component, Path},
};

use rustix::{
    fs::{Gid, Mode, OFlags, Uid},
    io::Errno,
};

/// An opened (and possibly just created) directory.
#[derive(Debug)]
pub struct EnsuredDir {
    /// `O_RDONLY | O_DIRECTORY` fd of the leaf.
    pub fd: OwnedFd,
    /// Whether the leaf itself was created by this call.
    pub created: bool,
}

/// Mode of directories `crabd init` creates (0755, before umask).
const DIR_MODE: Mode = Mode::RWXU
    .union(Mode::RGRP)
    .union(Mode::XGRP)
    .union(Mode::ROTH)
    .union(Mode::XOTH);

fn open_dir_nofollow(parent: impl AsFd, name: &str) -> Result<OwnedFd, Errno> {
    rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

fn open_error(e: Errno, shown: &Path) -> io::Error {
    match e {
        Errno::LOOP | Errno::NOTDIR => io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "`{}` is a symlink or not a directory; refusing to follow it",
                shown.display()
            ),
        ),
        e => {
            let e = io::Error::from(e);
            io::Error::new(e.kind(), format!("open {}: {e}", shown.display()))
        }
    }
}

/// Open `path` (absolute, already normalised: no `.`/`..`) below `root`, creating missing
/// components with mode 0755. `root` itself is trusted and resolved normally (production:
/// `/`); no component below it may be a symlink.
pub fn ensure_dir_in(root: &Path, path: &Path) -> io::Result<EnsuredDir> {
    // `root` is trusted, so it resolves normally (e.g. macOS `/var` -> `/private/var`).
    let mut cur = rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| {
        let e = io::Error::from(e);
        io::Error::new(e.kind(), format!("open {}: {e}", root.display()))
    })?;
    if !path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{}` is not a normalised absolute path", path.display()),
        ));
    }
    let mut shown = root.to_path_buf();
    let mut created = false;
    for c in path.components() {
        let name = match c {
            Component::Normal(n) => n.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "path is not valid UTF-8")
            })?,
            _ => continue,
        };
        shown.push(name);
        created = false;
        // Open first, create only on ENOENT: an existing component (even on a read-only
        // filesystem) is never touched.
        cur = match open_dir_nofollow(&cur, name) {
            Err(Errno::NOENT) => {
                match rustix::fs::mkdirat(&cur, name, DIR_MODE) {
                    Ok(()) => created = true,
                    // Lost a race: whatever is there now gets checked by the open below.
                    Err(Errno::EXIST) => {}
                    Err(e) => {
                        let e = io::Error::from(e);
                        return Err(io::Error::new(
                            e.kind(),
                            format!("mkdir {}: {e}", shown.display()),
                        ));
                    }
                }
                open_dir_nofollow(&cur, name).map_err(|e| open_error(e, &shown))?
            }
            r => r.map_err(|e| open_error(e, &shown))?,
        };
    }
    Ok(EnsuredDir { fd: cur, created })
}

/// `fchown` an opened directory.
pub fn chown_fd(fd: impl AsFd, uid: u32, gid: u32) -> io::Result<()> {
    rustix::fs::fchown(fd, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
        .map_err(|e| io::Error::new(io::Error::from(e).kind(), format!("fchown: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn creates_missing_components_and_reports_leaf_creation() {
        let t = tempfile::tempdir().unwrap();
        let d = ensure_dir_in(t.path(), Path::new("/a/b/c")).unwrap();
        assert!(d.created);
        assert!(t.path().join("a/b/c").is_dir());
        let again = ensure_dir_in(t.path(), Path::new("/a/b/c")).unwrap();
        assert!(!again.created);
        // Existing leaf, created parent: only the leaf counts.
        fs::create_dir_all(t.path().join("x/y")).unwrap();
        assert!(!ensure_dir_in(t.path(), Path::new("/x/y")).unwrap().created);
        assert!(
            ensure_dir_in(t.path(), Path::new("/x/y/z"))
                .unwrap()
                .created
        );
        // The root itself.
        assert!(!ensure_dir_in(t.path(), Path::new("/")).unwrap().created);
    }

    #[test]
    fn refuses_symlinks_anywhere_below_root() {
        let t = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("ws")).unwrap();
        // A previous turn planted links inside the (persistent) workspace.
        symlink(outside.path(), t.path().join("ws/data")).unwrap();
        symlink(outside.path().join("new"), t.path().join("ws/dangling")).unwrap();
        fs::write(t.path().join("ws/file"), b"x").unwrap();

        for (p, why) in [
            ("/ws/data", "symlink"),
            ("/ws/data/sub", "symlink"),
            ("/ws/dangling", "symlink"),
            ("/ws/dangling/sub", "symlink"),
            ("/ws/file", "not a directory"),
            ("/ws/file/sub", "not a directory"),
        ] {
            let err = ensure_dir_in(t.path(), Path::new(p)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{p}: {err}");
            assert!(err.to_string().contains(why), "{p}: {err}");
        }
        // Nothing was created through the links.
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn rejects_unnormalised_paths() {
        let t = tempfile::tempdir().unwrap();
        let err = ensure_dir_in(t.path(), Path::new("/a/../b")).unwrap_err();
        assert!(err.to_string().contains("normalised"), "{err}");
        assert!(!t.path().join("a").exists());
        assert!(ensure_dir_in(t.path(), Path::new("rel")).is_err());
    }

    #[test]
    fn chown_fd_to_self_succeeds() {
        let t = tempfile::tempdir().unwrap();
        let d = ensure_dir_in(t.path(), Path::new("/o")).unwrap();
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        chown_fd(&d.fd, uid, gid).unwrap();
    }
}
