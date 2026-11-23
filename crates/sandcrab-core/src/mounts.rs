//! Mount resolution: `SandboxConfig` mounts + registry → validated guest mount table.
//!
//! [`resolve_mounts`] is pure (registry access goes through a lookup closure) so the whole
//! rule matrix is unit-tested without touching the filesystem.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    MountMode, Result, RootIdentity, SandboxError, Workspace, WorkspaceBacking, WorkspaceId,
    WorkspaceMount, WorkspaceRef,
};

/// Guest paths that may never be a mount target (or contain one): kernel pseudo-filesystems
/// and Sandcrab's own runtime/binary locations inside the guest.
pub const FORBIDDEN_GUEST_PREFIXES: &[&str] =
    &["/proc", "/sys", "/dev", "/run/sandcrab", "/opt/sandcrab"];

/// One workspace mount, ready for a backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedMount {
    pub workspace_id: WorkspaceId,
    pub workspace_name: String,
    pub backing: WorkspaceBacking,
    /// Host directory (or image file for a block volume). The manager replaces it with the
    /// canonical path from the workspace lease.
    pub host_path: PathBuf,
    /// Absolute, normalized guest path.
    pub guest_path: PathBuf,
    pub mode: MountMode,
    /// Workspace generation at resolution / lease time.
    pub generation: u64,
    /// Identity of `host_path` observed at lease time (dir-backed workspaces). Backends
    /// re-check it with [`crate::verify_workspace_root`] right before using `host_path`.
    #[serde(default)]
    pub root_identity: Option<RootIdentity>,
}

/// Result of [`resolve_mounts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMounts {
    pub mounts: Vec<ResolvedMount>,
    pub default_cwd: PathBuf,
}

/// Lexically normalize an absolute guest path: must be absolute UTF-8 without NUL or `..`;
/// `.` and repeated slashes are dropped.
pub fn normalize_guest_path(path: &Path) -> Result<PathBuf> {
    let bad =
        |why: &str| SandboxError::InvalidConfig(format!("guest path `{}` {why}", path.display()));
    let s = path.to_str().ok_or_else(|| bad("is not valid UTF-8"))?;
    if s.contains('\0') {
        return Err(bad("contains a NUL byte"));
    }
    if !path.is_absolute() {
        return Err(bad("must be absolute"));
    }
    let mut out = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir => return Err(bad("must not contain `..`")),
            Component::Prefix(_) => return Err(bad("must be a POSIX path")),
        }
    }
    Ok(out)
}

/// Normalize and check a mount target: not `/`, not under a forbidden prefix.
pub fn validate_mount_target(path: &Path) -> Result<PathBuf> {
    let norm = normalize_guest_path(path)?;
    if norm == Path::new("/") {
        return Err(SandboxError::InvalidConfig(
            "cannot mount a workspace over the guest root `/`".into(),
        ));
    }
    for prefix in FORBIDDEN_GUEST_PREFIXES {
        if norm.starts_with(prefix) {
            return Err(SandboxError::InvalidConfig(format!(
                "cannot mount a workspace at `{}`: `{prefix}` is reserved",
                norm.display()
            )));
        }
    }
    Ok(norm)
}

/// Resolve and validate mounts.
///
/// Rules: every target passes [`validate_mount_target`]; no two targets are equal or nested
/// (`/workspace` + `/workspace/a` is an error, since a later mount would shadow part of an
/// earlier one and FC/Docker would order them differently); a workspace mounted read-write
/// may not be mounted a second time. `default_cwd` precedence: `default_cwd` argument →
/// first read-write mount's guest path → `template_workdir`; it must be absolute but may lie
/// outside every mount.
pub fn resolve_mounts(
    mounts: &[WorkspaceMount],
    default_cwd: Option<&Path>,
    template_workdir: &Path,
    lookup: impl Fn(&WorkspaceRef) -> Result<Workspace>,
) -> Result<ResolvedMounts> {
    let mut resolved: Vec<ResolvedMount> = Vec::with_capacity(mounts.len());
    for m in mounts {
        let guest_path = validate_mount_target(&m.guest_path)?;
        let ws = lookup(&m.workspace)?;
        for prev in &resolved {
            if prev.guest_path == guest_path {
                return Err(SandboxError::InvalidConfig(format!(
                    "two mounts target `{}`",
                    guest_path.display()
                )));
            }
            if guest_path.starts_with(&prev.guest_path) || prev.guest_path.starts_with(&guest_path)
            {
                return Err(SandboxError::InvalidConfig(format!(
                    "mount targets `{}` and `{}` are nested; use disjoint paths",
                    prev.guest_path.display(),
                    guest_path.display()
                )));
            }
            if prev.workspace_id == ws.id && (prev.mode.is_writable() || m.mode.is_writable()) {
                return Err(SandboxError::InvalidConfig(format!(
                    "workspace `{}` is mounted more than once with a read-write mount; \
                     a read-write mount must be its only mount",
                    ws.name
                )));
            }
        }
        resolved.push(ResolvedMount {
            workspace_id: ws.id,
            workspace_name: ws.name.clone(),
            host_path: ws.backing.path().to_path_buf(),
            backing: ws.backing,
            guest_path,
            mode: m.mode,
            generation: ws.generation,
            root_identity: ws.root_identity,
        });
    }

    let default_cwd = match default_cwd {
        Some(cwd) => normalize_guest_path(cwd)
            .map_err(|e| SandboxError::InvalidConfig(format!("default_cwd: {e}")))?,
        None => match resolved.iter().find(|m| m.mode.is_writable()) {
            Some(m) => m.guest_path.clone(),
            None => normalize_guest_path(template_workdir)?,
        },
    };

    Ok(ResolvedMounts {
        mounts: resolved,
        default_cwd,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;

    use super::*;
    use crate::WorkspaceState;

    fn ws(name: &str) -> Workspace {
        Workspace {
            id: WorkspaceId::new(),
            name: name.into(),
            backing: WorkspaceBacking::HostDir(PathBuf::from(format!("/host/{name}"))),
            generation: 7,
            quota_mb: None,
            state: WorkspaceState::Idle,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            root_identity: None,
        }
    }

    struct Reg(HashMap<String, Workspace>);

    impl Reg {
        fn new(names: &[&str]) -> Self {
            Self(names.iter().map(|n| (n.to_string(), ws(n))).collect())
        }

        fn lookup(&self) -> impl Fn(&WorkspaceRef) -> Result<Workspace> + '_ {
            |r| match r {
                WorkspaceRef::Name(n) => self
                    .0
                    .get(n)
                    .cloned()
                    .ok_or_else(|| SandboxError::not_found("workspace", n)),
                WorkspaceRef::Id(id) => self
                    .0
                    .values()
                    .find(|w| w.id == *id)
                    .cloned()
                    .ok_or_else(|| SandboxError::not_found("workspace", id)),
            }
        }
    }

    fn m(ws: &str, guest: &str, mode: MountMode) -> WorkspaceMount {
        WorkspaceMount {
            workspace: ws.into(),
            guest_path: guest.into(),
            mode,
        }
    }

    use MountMode::{ReadOnly as RO, ReadWrite as RW};

    fn resolve(reg: &Reg, mounts: &[WorkspaceMount], cwd: Option<&str>) -> Result<ResolvedMounts> {
        resolve_mounts(
            mounts,
            cwd.map(Path::new),
            Path::new("/workspace"),
            reg.lookup(),
        )
    }

    #[test]
    fn normalization() {
        assert_eq!(
            normalize_guest_path(Path::new("/a//b/./c/")).unwrap(),
            PathBuf::from("/a/b/c")
        );
        for bad in ["a/b", "", "/a/../b", "/a/..", "relative"] {
            assert!(normalize_guest_path(Path::new(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn forbidden_targets() {
        for bad in [
            "/",
            "//",
            "/.",
            "/proc",
            "/proc/self",
            "/sys/fs",
            "/dev",
            "/dev/shm",
            "/run/sandcrab",
            "/run/sandcrab/x",
            "/opt/sandcrab",
        ] {
            assert!(validate_mount_target(Path::new(bad)).is_err(), "{bad}");
        }
        for ok in [
            "/workspace",
            "/run/user",
            "/opt/sandcrabby",
            "/devices",
            "/home/u/x",
        ] {
            validate_mount_target(Path::new(ok)).unwrap();
        }
    }

    #[test]
    fn happy_path_and_host_paths() {
        let reg = Reg::new(&["code", "data"]);
        let r = resolve(
            &reg,
            &[m("data", "/data/", RO), m("code", "/workspace", RW)],
            None,
        )
        .unwrap();
        assert_eq!(r.mounts.len(), 2);
        assert_eq!(r.mounts[0].guest_path, PathBuf::from("/data"));
        assert_eq!(r.mounts[0].host_path, PathBuf::from("/host/data"));
        assert_eq!(r.mounts[0].mode, RO);
        assert_eq!(r.mounts[1].workspace_name, "code");
        assert_eq!(r.mounts[1].generation, 7);
        // First rw mount wins over template workdir.
        assert_eq!(r.default_cwd, PathBuf::from("/workspace"));
    }

    #[test]
    fn default_cwd_precedence() {
        let reg = Reg::new(&["a", "b"]);
        // explicit
        let r = resolve(&reg, &[m("a", "/src", RW)], Some("/tmp/./x")).unwrap();
        assert_eq!(r.default_cwd, PathBuf::from("/tmp/x"));
        // first rw mount (skipping ro ones)
        let r = resolve(&reg, &[m("a", "/ro", RO), m("b", "/rw", RW)], None).unwrap();
        assert_eq!(r.default_cwd, PathBuf::from("/rw"));
        // template workdir when only ro mounts
        let r = resolve(&reg, &[m("a", "/ro", RO)], None).unwrap();
        assert_eq!(r.default_cwd, PathBuf::from("/workspace"));
        // no mounts at all
        let r = resolve(&reg, &[], None).unwrap();
        assert_eq!(r.default_cwd, PathBuf::from("/workspace"));
        // explicit cwd outside every mount is allowed but must be absolute
        assert!(resolve(&reg, &[m("a", "/src", RW)], Some("/elsewhere")).is_ok());
        assert!(resolve(&reg, &[m("a", "/src", RW)], Some("relative")).is_err());
        assert!(resolve(&reg, &[], Some("/a/../b")).is_err());
    }

    #[test]
    fn duplicate_and_nested_targets() {
        let reg = Reg::new(&["a", "b"]);
        for mounts in [
            vec![m("a", "/w", RO), m("b", "/w/", RO)],
            vec![m("a", "/w", RO), m("b", "/w/sub", RO)],
            vec![m("a", "/w/sub", RO), m("b", "/w", RO)],
            vec![m("a", "/w/./sub", RO), m("b", "/w//sub/deeper", RO)],
        ] {
            assert!(
                matches!(
                    resolve(&reg, &mounts, None),
                    Err(SandboxError::InvalidConfig(_))
                ),
                "{mounts:?}"
            );
        }
        // Siblings sharing a string prefix are not nested.
        resolve(&reg, &[m("a", "/w", RO), m("b", "/w2", RO)], None).unwrap();
    }

    #[test]
    fn same_workspace_twice() {
        let reg = Reg::new(&["a"]);
        resolve(&reg, &[m("a", "/x", RO), m("a", "/y", RO)], None).unwrap();
        for mounts in [
            vec![m("a", "/x", RW), m("a", "/y", RO)],
            vec![m("a", "/x", RO), m("a", "/y", RW)],
            vec![m("a", "/x", RW), m("a", "/y", RW)],
        ] {
            assert!(resolve(&reg, &mounts, None).is_err(), "{mounts:?}");
        }
        // Same workspace referenced by id and by name is still the same workspace.
        let id = reg.0["a"].id;
        let by_id = WorkspaceMount {
            workspace: id.into(),
            guest_path: "/y".into(),
            mode: RO,
        };
        assert!(resolve(&reg, &[m("a", "/x", RW), by_id], None).is_err());
    }

    #[test]
    fn invalid_targets_and_unknown_workspace() {
        let reg = Reg::new(&["a"]);
        for target in ["relative", "/", "/proc", "/dev/x", "/a/../etc"] {
            assert!(
                resolve(&reg, &[m("a", target, RW)], None).is_err(),
                "{target}"
            );
        }
        assert!(matches!(
            resolve(&reg, &[m("missing", "/x", RW)], None),
            Err(SandboxError::NotFound { .. })
        ));
    }
}
