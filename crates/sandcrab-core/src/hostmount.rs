//! Ad-hoc host mounts: a host directory mounted at the **same** path in the sandbox, allowed
//! only beneath configured roots (design §6.1, §6.2).
//!
//! These coexist with registered workspaces ([`crate::WorkspaceRegistry`]) and share none of
//! their machinery: no registry entry, no lease, no rule about nesting across sandboxes.
//!
//! [`MountPolicy::new`] resolves each root with `realpath` once and holds a descriptor to it.
//! [`MountPolicy::resolve`] then opens every mount beneath that descriptor one component at a
//! time with `O_NOFOLLOW`, so a symlink anywhere below the root is refused rather than
//! followed, and records the `(dev, ino)` of every directory on the way. Protected
//! directories are compared by `(dev, ino)`, so a second name for one (a bind mount, a
//! symlinked parent) does not get around the check.

use std::ffi::OsStr;
use std::fmt;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{AtFlags, FileType, Mode};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::fsx::{DIR_FLAGS, identity};
use crate::{
    MountMode, ResolvedMount, Result, RootIdentity, SandboxError, WorkspaceBacking, WorkspaceId,
    validate_mount_target,
};

/// Mounts one sandbox may have, host mounts and workspace mounts together (the libkrun
/// helper's share limit, applied on every backend).
pub const MAX_SANDBOX_MOUNTS: usize = 16;

/// `(st_dev, st_ino)` of a directory.
type Ident = (u64, u64);

/// A host directory to mount at the same absolute path in the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostMount {
    /// Absolute, symlink-free host path; also the guest path.
    pub path: PathBuf,
    pub mode: MountMode,
}

impl HostMount {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, mode: MountMode) -> Self {
        Self {
            path: path.into(),
            mode,
        }
    }
}

/// An allowed root for host mounts and the highest mode a mount beneath it may have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRoot {
    pub path: PathBuf,
    pub max_mode: MountMode,
}

/// The allowed roots and protected directories that host mounts are checked against. Cheap to
/// clone; the root descriptors are shared.
#[derive(Clone)]
pub struct MountPolicy {
    inner: Arc<PolicyInner>,
}

struct PolicyInner {
    roots: Vec<HeldRoot>,
    protected: Vec<PathBuf>,
}

struct HeldRoot {
    /// `realpath` of the configured root.
    path: PathBuf,
    max_mode: MountMode,
    fd: OwnedFd,
    /// `(dev, ino)` of `/`, every ancestor, and the root itself.
    chain: Vec<Ident>,
}

impl fmt::Debug for MountPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MountPolicy")
            .field("roots", &self.roots())
            .field("protected", &self.inner.protected)
            .finish()
    }
}

fn refused(path: &Path, reason: impl Into<String>) -> SandboxError {
    SandboxError::MountNotAllowed {
        path: path.to_path_buf(),
        reason: reason.into(),
        cap: None,
    }
}

impl MountPolicy {
    /// Resolve every root with `realpath` (on macOS `/tmp` is a symlink) and open it. A root
    /// that does not exist or is not a directory is an error. `protected` names directories
    /// no mount may be, contain, or lie inside (the server's socket and state directories,
    /// the pin store, ...); they are looked up again at every check and may not exist yet.
    pub fn new(roots: Vec<MountRoot>, protected: Vec<PathBuf>) -> Result<Self> {
        let mut held = Vec::with_capacity(roots.len());
        for root in roots {
            if !root.path.is_absolute() {
                return Err(SandboxError::InvalidConfig(format!(
                    "mount root `{}` must be absolute",
                    root.path.display()
                )));
            }
            let path = std::fs::canonicalize(&root.path)
                .map_err(|e| SandboxError::io_at(&root.path, e))?;
            let (fd, chain) = open_chain(&path).map_err(|e| {
                SandboxError::InvalidConfig(format!(
                    "mount root `{}`: {}",
                    root.path.display(),
                    e.reason()
                ))
            })?;
            held.push(HeldRoot {
                path,
                max_mode: root.max_mode,
                fd,
                chain,
            });
        }
        for p in &protected {
            if !p.is_absolute() {
                return Err(SandboxError::InvalidConfig(format!(
                    "protected directory `{}` must be absolute",
                    p.display()
                )));
            }
        }
        Ok(Self {
            inner: Arc::new(PolicyInner {
                roots: held,
                protected,
            }),
        })
    }

    /// The resolved roots and their highest mode.
    #[must_use]
    pub fn roots(&self) -> Vec<MountRoot> {
        self.inner
            .roots
            .iter()
            .map(|r| MountRoot {
                path: r.path.clone(),
                max_mode: r.max_mode,
            })
            .collect()
    }

    /// Check `mounts` (design §6.2) and resolve them for a backend. `taken` are the guest
    /// paths of the sandbox's other mounts (registered workspaces): no two paths of one
    /// sandbox may be equal or nested, and all of them count against
    /// [`MAX_SANDBOX_MOUNTS`].
    pub fn resolve(&self, mounts: &[HostMount], taken: &[&Path]) -> Result<Vec<ResolvedMount>> {
        if let Some(m) = mounts.get(MAX_SANDBOX_MOUNTS.saturating_sub(taken.len())) {
            return Err(SandboxError::MountNotAllowed {
                path: m.path.clone(),
                reason: "too many mounts".into(),
                cap: Some(MAX_SANDBOX_MOUNTS),
            });
        }
        let protected = self.protected_chains();
        let mut out: Vec<ResolvedMount> = Vec::with_capacity(mounts.len());
        for m in mounts {
            let path = validate_mount_target(&m.path)?;
            let others = taken
                .iter()
                .copied()
                .chain(out.iter().map(|r| r.guest_path.as_path()));
            for other in others {
                if path.starts_with(other) || other.starts_with(&path) {
                    return Err(refused(
                        &path,
                        format!(
                            "it is equal to or nested with `{}`; mounts of one sandbox must be \
                             disjoint",
                            other.display()
                        ),
                    ));
                }
            }
            let (ident, chain) = self.open(&path, m.mode)?;
            for (pid, pchain) in &protected {
                if chain.contains(pid) || pchain.contains(&ident) {
                    return Err(refused(
                        &path,
                        "it is, contains or lies inside a protected directory",
                    ));
                }
            }
            out.push(ResolvedMount {
                workspace_id: WorkspaceId::new(),
                workspace_name: path.display().to_string(),
                backing: WorkspaceBacking::HostDir(path.clone()),
                host_path: path.clone(),
                guest_path: path,
                mode: m.mode,
                generation: 0,
                root_identity: Some(RootIdentity {
                    dev: ident.0,
                    ino: ident.1,
                }),
            });
        }
        Ok(out)
    }

    /// Open `path` beneath its allowed root: `(identity, chain from / to path)`.
    fn open(&self, path: &Path, mode: MountMode) -> Result<(Ident, Vec<Ident>)> {
        let root = self
            .inner
            .roots
            .iter()
            .filter(|r| path.starts_with(&r.path))
            .max_by_key(|r| r.path.components().count())
            .ok_or_else(|| refused(path, "it is not beneath an allowed mount root"))?;
        if mode.is_writable() && !root.max_mode.is_writable() {
            return Err(refused(
                path,
                format!(
                    "read-write is not allowed beneath the read-only root `{}`",
                    root.path.display()
                ),
            ));
        }
        let rel = path
            .strip_prefix(&root.path)
            .expect("checked by starts_with");
        let mut chain = root.chain.clone();
        let mut cur: Option<OwnedFd> = None;
        for name in rel.components().filter_map(|c| match c {
            Component::Normal(n) => Some(n),
            _ => None,
        }) {
            let dir = cur.as_ref().unwrap_or(&root.fd);
            let next = open_dir(dir, name).map_err(|e| refused(path, e.reason()))?;
            chain.push(identity(&next).map_err(|e| SandboxError::io_at(path, e))?);
            cur = Some(next);
        }
        let ident = *chain.last().expect("the chain holds at least the root");
        Ok((ident, chain))
    }

    /// `(identity, chain from /)` of every protected directory that exists now.
    fn protected_chains(&self) -> Vec<(Ident, Vec<Ident>)> {
        self.inner
            .protected
            .iter()
            .filter_map(|p| {
                let real = std::fs::canonicalize(p).ok()?;
                let (_, chain) = open_chain(&real).ok()?;
                Some((*chain.last()?, chain))
            })
            .collect()
    }
}

/// Why opening one directory failed.
enum OpenError {
    Symlink,
    NotADirectory,
    Missing,
    Io(std::io::Error),
}

impl OpenError {
    fn reason(&self) -> String {
        match self {
            Self::Symlink => "a path component is a symlink".into(),
            Self::NotADirectory => "a path component is not a directory".into(),
            Self::Missing => "it does not exist".into(),
            Self::Io(e) => format!("cannot open it: {e}"),
        }
    }
}

fn open_dir(dir: &OwnedFd, name: &OsStr) -> std::result::Result<OwnedFd, OpenError> {
    match rustix::fs::openat(dir, name, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => Ok(fd),
        Err(Errno::LOOP) => Err(OpenError::Symlink),
        // Linux reports `O_DIRECTORY | O_NOFOLLOW` on a symlink as ENOTDIR.
        Err(Errno::NOTDIR) => {
            let link = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Symlink);
            Err(if link {
                OpenError::Symlink
            } else {
                OpenError::NotADirectory
            })
        }
        Err(Errno::NOENT) => Err(OpenError::Missing),
        Err(e) => Err(OpenError::Io(e.into())),
    }
}

/// Open an absolute, canonical path from `/` without following symlinks: the final
/// descriptor and the `(dev, ino)` of `/` and every component.
fn open_chain(path: &Path) -> std::result::Result<(OwnedFd, Vec<Ident>), OpenError> {
    let mut cur = rustix::fs::openat(rustix::fs::CWD, "/", DIR_FLAGS, Mode::empty())
        .map_err(|e| OpenError::Io(e.into()))?;
    let mut chain = vec![identity(&cur).map_err(OpenError::Io)?];
    for c in path.components() {
        let Component::Normal(name) = c else {
            continue;
        };
        cur = open_dir(&cur, name)?;
        chain.push(identity(&cur).map_err(OpenError::Io)?);
    }
    Ok((cur, chain))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;
    use MountMode::{ReadOnly as RO, ReadWrite as RW};

    struct Env {
        _tmp: tempfile::TempDir,
        /// Canonical base directory.
        base: PathBuf,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        for d in ["rw/a/b", "rw/c", "rw/state", "ro/x", "ro/inner", "outside"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        fs::write(base.join("rw/file"), b"x").unwrap();
        Env { _tmp: tmp, base }
    }

    fn policy(env: &Env, protected: &[&str]) -> MountPolicy {
        MountPolicy::new(
            vec![
                MountRoot {
                    path: env.base.join("rw"),
                    max_mode: RW,
                },
                MountRoot {
                    path: env.base.join("ro"),
                    max_mode: RO,
                },
            ],
            protected.iter().map(|p| env.base.join(p)).collect(),
        )
        .unwrap()
    }

    fn reason(r: Result<Vec<ResolvedMount>>) -> String {
        match r {
            Err(SandboxError::MountNotAllowed { reason, .. }) => reason,
            other => panic!("expected MountNotAllowed, got {other:?}"),
        }
    }

    #[test]
    fn resolves_beneath_roots_with_identity() {
        let env = env();
        let p = policy(&env, &[]);
        let path = env.base.join("rw/a/b");
        let r = p.resolve(&[HostMount::new(&path, RW)], &[]).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].host_path, path);
        assert_eq!(r[0].guest_path, path);
        assert_eq!(r[0].mode, RW);
        let st = fs::metadata(&path).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            r[0].root_identity,
            Some(RootIdentity {
                dev: st.dev(),
                ino: st.ino()
            })
        );
        // The root itself, and ro beneath a ro root.
        p.resolve(&[HostMount::new(env.base.join("rw"), RW)], &[])
            .unwrap();
        p.resolve(&[HostMount::new(env.base.join("ro/x"), RO)], &[])
            .unwrap();
    }

    #[test]
    fn outside_roots_and_mode_cap() {
        let env = env();
        let p = policy(&env, &[]);
        for path in [env.base.join("outside"), env.base.clone(), "/".into()] {
            assert!(
                p.resolve(&[HostMount::new(&path, RO)], &[]).is_err(),
                "{path:?}"
            );
        }
        // A sibling sharing a string prefix is not beneath the root.
        fs::create_dir(env.base.join("rw2")).unwrap();
        assert!(
            reason(p.resolve(&[HostMount::new(env.base.join("rw2"), RO)], &[])).contains("root")
        );
        assert!(
            reason(p.resolve(&[HostMount::new(env.base.join("ro/x"), RW)], &[]))
                .contains("read-only")
        );
        // No roots at all: everything is refused.
        let none = MountPolicy::new(Vec::new(), Vec::new()).unwrap();
        reason(none.resolve(&[HostMount::new(env.base.join("rw/a"), RO)], &[]));
    }

    #[test]
    fn longest_root_wins() {
        let env = env();
        let p = MountPolicy::new(
            vec![
                MountRoot {
                    path: env.base.join("ro"),
                    max_mode: RW,
                },
                MountRoot {
                    path: env.base.join("ro/inner"),
                    max_mode: RO,
                },
            ],
            Vec::new(),
        )
        .unwrap();
        p.resolve(&[HostMount::new(env.base.join("ro/x"), RW)], &[])
            .unwrap();
        reason(p.resolve(&[HostMount::new(env.base.join("ro/inner"), RW)], &[]));
    }

    #[test]
    fn symlink_at_each_depth_is_refused() {
        let env = env();
        let p = policy(&env, &[]);
        let rw = env.base.join("rw");
        symlink(rw.join("a"), rw.join("link1")).unwrap();
        symlink(rw.join("a/b"), rw.join("a/link2")).unwrap();
        symlink(env.base.join("outside"), rw.join("a/b/link3")).unwrap();
        for bad in ["link1", "link1/b", "a/link2", "a/b/link3"] {
            let r = reason(p.resolve(&[HostMount::new(rw.join(bad), RO)], &[]));
            assert!(r.contains("symlink"), "{bad}: {r}");
        }
        assert!(
            reason(p.resolve(&[HostMount::new(rw.join("file"), RO)], &[]))
                .contains("not a directory")
        );
        assert!(
            reason(p.resolve(&[HostMount::new(rw.join("missing"), RO)], &[]))
                .contains("does not exist")
        );
        // `..` never gets past normalisation.
        assert!(matches!(
            p.resolve(&[HostMount::new(rw.join("a/../c"), RO)], &[]),
            Err(SandboxError::InvalidConfig(_))
        ));
        // Relative and reserved paths too.
        for bad in ["rel/path", "/proc/self"] {
            assert!(p.resolve(&[HostMount::new(bad, RO)], &[]).is_err(), "{bad}");
        }
    }

    #[test]
    fn symlinked_root_is_resolved_once() {
        let env = env();
        // Like macOS `/tmp` -> `/private/tmp`.
        symlink(env.base.join("rw"), env.base.join("alias")).unwrap();
        let p = MountPolicy::new(
            vec![MountRoot {
                path: env.base.join("alias"),
                max_mode: RW,
            }],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(p.roots()[0].path, env.base.join("rw"));
        p.resolve(&[HostMount::new(env.base.join("rw/a"), RW)], &[])
            .unwrap();
        // The unresolved name has a symlink in it: refused.
        reason(p.resolve(&[HostMount::new(env.base.join("alias/a"), RW)], &[]));
        // A missing root is a configuration error.
        assert!(
            MountPolicy::new(
                vec![MountRoot {
                    path: env.base.join("nope"),
                    max_mode: RO,
                }],
                Vec::new(),
            )
            .is_err()
        );
    }

    #[test]
    fn nesting_inside_one_sandbox_is_refused() {
        let env = env();
        let p = policy(&env, &[]);
        let a = env.base.join("rw/a");
        let ab = env.base.join("rw/a/b");
        for mounts in [
            vec![HostMount::new(&a, RO), HostMount::new(&ab, RO)],
            vec![HostMount::new(&ab, RO), HostMount::new(&a, RO)],
            vec![HostMount::new(&a, RO), HostMount::new(&a, RW)],
        ] {
            assert!(
                reason(p.resolve(&mounts, &[])).contains("nested"),
                "{mounts:?}"
            );
        }
        // Against a workspace mount of the same sandbox.
        reason(p.resolve(&[HostMount::new(&ab, RO)], &[a.as_path()]));
        // Disjoint siblings are fine; so are nested folders in two separate sandboxes.
        p.resolve(
            &[
                HostMount::new(&a, RO),
                HostMount::new(env.base.join("rw/c"), RW),
            ],
            &[],
        )
        .unwrap();
        p.resolve(&[HostMount::new(&a, RW)], &[]).unwrap();
        p.resolve(&[HostMount::new(&ab, RW)], &[]).unwrap();
    }

    #[test]
    fn mount_cap_names_the_cap() {
        let env = env();
        let p = policy(&env, &[]);
        let mut mounts = Vec::new();
        for i in 0..=MAX_SANDBOX_MOUNTS {
            let d = env.base.join(format!("rw/m{i}"));
            fs::create_dir(&d).unwrap();
            mounts.push(HostMount::new(d, RO));
        }
        let err = p.resolve(&mounts, &[]).unwrap_err();
        assert!(
            matches!(&err, SandboxError::MountNotAllowed { cap: Some(MAX_SANDBOX_MOUNTS), reason, .. }
                if reason == "too many mounts"),
            "{err:?}"
        );
        assert!(err.to_string().contains("cap: 16"), "{err}");
        p.resolve(&mounts[..MAX_SANDBOX_MOUNTS], &[]).unwrap();
        // Workspace mounts count too.
        let ws = Path::new("/workspace");
        assert!(p.resolve(&mounts[..MAX_SANDBOX_MOUNTS], &[ws]).is_err());
    }

    #[test]
    fn protected_directories_by_identity() {
        let env = env();
        // Protected given through a symlinked name; mounts reach it by its real path.
        symlink(env.base.join("rw/state"), env.base.join("state-link")).unwrap();
        let p = policy(&env, &["state-link"]);
        let rw = env.base.join("rw");
        fs::create_dir(rw.join("state/sock")).unwrap();
        // Is it, lies inside it, contains it.
        for bad in [rw.join("state"), rw.join("state/sock"), rw.clone()] {
            let r = reason(p.resolve(&[HostMount::new(&bad, RO)], &[]));
            assert!(r.contains("protected"), "{bad:?}: {r}");
        }
        p.resolve(&[HostMount::new(rw.join("a"), RW)], &[]).unwrap();

        // Protected named with `.` and `..` in it: the same directory, so still refused.
        let p = policy(&env, &["rw/c/../a/./b"]);
        reason(p.resolve(&[HostMount::new(rw.join("a/b"), RO)], &[]));
        reason(p.resolve(&[HostMount::new(rw.join("a"), RO)], &[]));
        p.resolve(&[HostMount::new(rw.join("c"), RO)], &[]).unwrap();
        // A protected directory that does not exist (yet) protects nothing.
        let p = policy(&env, &["not-there"]);
        p.resolve(&[HostMount::new(rw.join("a"), RW)], &[]).unwrap();
    }
}
