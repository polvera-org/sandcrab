//! Persistent workspace model: ids, references, backings, mount modes and safe relative paths.
//!
//! The registry that persists these lives in [`crate::registry`].

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Result, SandboxError, SandboxId};

/// Unique identifier for a persistent workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkspaceId(pub Uuid);

impl WorkspaceId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for WorkspaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for WorkspaceId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

/// Reference to a workspace by ID or registered name.
///
/// Workspace names can never parse as a UUID, so converting a string with
/// [`WorkspaceRef::parse`] (or `From<&str>`) is unambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkspaceRef {
    Id(WorkspaceId),
    Name(String),
}

impl WorkspaceRef {
    /// A UUID string becomes [`WorkspaceRef::Id`], anything else a [`WorkspaceRef::Name`].
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match WorkspaceId::from_str(s) {
            Ok(id) => Self::Id(id),
            Err(_) => Self::Name(s.to_owned()),
        }
    }

    /// True if this reference designates `ws`.
    #[must_use]
    pub fn matches(&self, ws: &Workspace) -> bool {
        match self {
            Self::Id(id) => ws.id == *id,
            Self::Name(name) => ws.name == *name,
        }
    }
}

impl fmt::Display for WorkspaceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id(id) => id.fmt(f),
            Self::Name(name) => f.write_str(name),
        }
    }
}

impl From<WorkspaceId> for WorkspaceRef {
    fn from(id: WorkspaceId) -> Self {
        Self::Id(id)
    }
}

impl From<&WorkspaceId> for WorkspaceRef {
    fn from(id: &WorkspaceId) -> Self {
        Self::Id(*id)
    }
}

impl From<&Workspace> for WorkspaceRef {
    fn from(ws: &Workspace) -> Self {
        Self::Id(ws.id)
    }
}

impl From<&str> for WorkspaceRef {
    fn from(s: &str) -> Self {
        Self::parse(s)
    }
}

impl From<String> for WorkspaceRef {
    fn from(s: String) -> Self {
        Self::parse(&s)
    }
}

/// Storage backing for a persistent workspace.
///
/// The host directory is the source of truth for `HostDir` / `ManagedDir` on every backend
/// (see `docs/design/firecracker-backend.md` §5): Docker bind-mounts it, Firecracker builds
/// an ext4 image from it per spawn and syncs back at teardown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
pub enum WorkspaceBacking {
    /// User-registered existing host directory (canonical path). Never deleted by Sandcrab.
    HostDir(PathBuf),
    /// Directory managed by Sandcrab under `<state_root>/workspaces/<id>/data`.
    ManagedDir(PathBuf),
    /// Persistent ext4 / dm-thin block volume (Firecracker & cloud). Reserved in V1:
    /// backends may attach it directly; host-side file ops are unimplemented.
    BlockVolume(PathBuf),
}

impl WorkspaceBacking {
    /// Host path of the backing directory or image.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::HostDir(p) | Self::ManagedDir(p) | Self::BlockVolume(p) => p,
        }
    }

    #[must_use]
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::HostDir(_) | Self::ManagedDir(_))
    }
}

/// Mount access mode enforced at the host/VMM boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountMode {
    ReadWrite,
    ReadOnly,
}

impl MountMode {
    #[must_use]
    pub fn is_writable(self) -> bool {
        self == Self::ReadWrite
    }
}

impl fmt::Display for MountMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ReadWrite => "rw",
            Self::ReadOnly => "ro",
        })
    }
}

impl FromStr for MountMode {
    type Err = SandboxError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "rw" | "read_write" => Ok(Self::ReadWrite),
            "ro" | "read_only" => Ok(Self::ReadOnly),
            other => Err(SandboxError::InvalidConfig(format!(
                "mount mode must be `rw` or `ro`, got `{other}`"
            ))),
        }
    }
}

/// Lease / session state of a workspace.
///
/// Leases are tracked by the owning [`crate::WorkspaceRegistry`] process; only `Idle` and
/// `DirtySession` are persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkspaceState {
    #[default]
    Idle,
    /// Exclusively mounted read-write by a sandbox; host mutation is refused.
    LeasedRw { sandbox: SandboxId },
    /// Mounted read-only by `holders` sandboxes.
    LeasedRo { holders: u32 },
    /// A previous rw session could not sync back; needs recovery before reuse.
    DirtySession,
}

/// A persistent workspace registered with Sandcrab.
/// Workspaces outlive ephemeral sandboxes and may be mutated between turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub backing: WorkspaceBacking,
    /// Bumped on every committed change (host mutation, rw lease release, sync-back).
    /// Backends key read-only image caches on it.
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub quota_mb: Option<u64>,
    #[serde(default)]
    pub state: WorkspaceState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// `(dev, ino)` of the backing directory, recorded at registration. Every host op and
    /// lease checks the root still is that directory. `None` for block volumes and for
    /// workspaces registered before this was recorded (the symlink-free walk still applies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_identity: Option<RootIdentity>,
}

/// Identity of a directory (`st_dev`, `st_ino`): detects a workspace root that was replaced
/// (renamed away, swapped for another directory) after registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RootIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// Mount instruction binding a workspace into a sandbox at spawn time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMount {
    pub workspace: WorkspaceRef,
    pub guest_path: PathBuf,
    pub mode: MountMode,
}

impl WorkspaceMount {
    /// Parse `<workspace>:<guest_path>[:rw|ro]` (default `rw`), the CLI `--mount` syntax.
    pub fn parse_spec(spec: &str) -> Result<Self> {
        let parts: Vec<&str> = spec.split(':').collect();
        let (ws, guest, mode) = match parts.as_slice() {
            [ws, guest] => (*ws, *guest, MountMode::ReadWrite),
            [ws, guest, mode] => (*ws, *guest, mode.parse()?),
            _ => {
                return Err(SandboxError::InvalidConfig(format!(
                    "mount spec `{spec}` must be `<workspace>:<guest_path>[:rw|ro]`"
                )));
            }
        };
        if ws.is_empty() || guest.is_empty() {
            return Err(SandboxError::InvalidConfig(format!(
                "mount spec `{spec}` has an empty workspace or guest path"
            )));
        }
        Ok(Self {
            workspace: WorkspaceRef::parse(ws),
            guest_path: PathBuf::from(guest),
            mode,
        })
    }
}

/// Validate a workspace name: `[a-z0-9][a-z0-9-_]{0,62}`, and not a UUID.
pub fn validate_workspace_name(name: &str) -> Result<()> {
    validate_slug("workspace", name)?;
    if Uuid::parse_str(name).is_ok() {
        return Err(SandboxError::InvalidConfig(format!(
            "workspace name `{name}` must not be a UUID (it would be ambiguous with an id)"
        )));
    }
    Ok(())
}

/// `[a-z0-9][a-z0-9-_]{0,62}`; shared by workspace and template names.
pub(crate) fn validate_slug(kind: &str, name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let ok = !bytes.is_empty()
        && bytes.len() <= 63
        && matches!(bytes[0], b'a'..=b'z' | b'0'..=b'9')
        && bytes
            .iter()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'));
    if ok {
        Ok(())
    } else {
        Err(SandboxError::InvalidConfig(format!(
            "{kind} name `{name}` must match [a-z0-9][a-z0-9-_]{{0,62}}"
        )))
    }
}

/// A validated relative path inside a workspace: no root, no prefix, no `..`, no NUL.
///
/// `.` components are dropped. The empty path designates the workspace root itself
/// (valid for directory listings, rejected for file operations).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SafeRelPath(PathBuf);

impl SafeRelPath {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(SandboxError::PathEscape(format!(
                "`{}` contains a NUL byte",
                path.display()
            )));
        }
        let mut out = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => out.push(part),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(SandboxError::PathEscape(format!(
                        "`{}` contains `..`",
                        path.display()
                    )));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(SandboxError::PathEscape(format!(
                        "`{}` is absolute; workspace paths must be relative",
                        path.display()
                    )));
                }
            }
        }
        Ok(Self(out))
    }

    /// The workspace root.
    #[must_use]
    pub fn root() -> Self {
        Self(PathBuf::new())
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.as_os_str().is_empty()
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, name: &str) -> Result<Self> {
        Self::new(self.0.join(name))
    }
}

impl AsRef<Path> for SafeRelPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for SafeRelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            f.write_str(".")
        } else {
            self.0.display().fmt(f)
        }
    }
}

impl TryFrom<&Path> for SafeRelPath {
    type Error = SandboxError;

    fn try_from(p: &Path) -> Result<Self> {
        Self::new(p)
    }
}

impl TryFrom<&str> for SafeRelPath {
    type Error = SandboxError;

    fn try_from(p: &str) -> Result<Self> {
        Self::new(p)
    }
}

impl FromStr for SafeRelPath {
    type Err = SandboxError;

    fn from_str(p: &str) -> Result<Self> {
        Self::new(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["a", "0", "my-ws", "proj_1", &"a".repeat(63)] {
            validate_workspace_name(ok).unwrap();
        }
        for bad in [
            "",
            "-a",
            "_a",
            "A",
            "a b",
            "a/b",
            "a.b",
            "ä",
            &"a".repeat(64),
            "67e55044-10b1-426f-9247-bb680e5fe0c8",
        ] {
            assert!(validate_workspace_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn workspace_ref_parse() {
        let id = WorkspaceId::new();
        assert_eq!(WorkspaceRef::parse(&id.to_string()), WorkspaceRef::Id(id));
        assert_eq!(
            WorkspaceRef::from("proj"),
            WorkspaceRef::Name("proj".into())
        );
        let json = serde_json::to_string(&WorkspaceRef::Id(id)).unwrap();
        assert_eq!(
            serde_json::from_str::<WorkspaceRef>(&json).unwrap(),
            WorkspaceRef::Id(id)
        );
    }

    #[test]
    fn safe_rel_path() {
        assert_eq!(
            SafeRelPath::new("a/./b").unwrap().as_path(),
            Path::new("a/b")
        );
        assert!(SafeRelPath::new("").unwrap().is_root());
        assert!(SafeRelPath::new(".").unwrap().is_root());
        assert!(SafeRelPath::new("./").unwrap().is_root());
        for bad in ["/etc/passwd", "../x", "a/../../x", "a/..", "a\0b"] {
            assert!(
                matches!(SafeRelPath::new(bad), Err(SandboxError::PathEscape(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn mount_spec() {
        let m = WorkspaceMount::parse_spec("proj:/workspace").unwrap();
        assert_eq!(m.mode, MountMode::ReadWrite);
        let m = WorkspaceMount::parse_spec("proj:/data:ro").unwrap();
        assert_eq!(m.mode, MountMode::ReadOnly);
        assert_eq!(m.guest_path, PathBuf::from("/data"));
        assert!(WorkspaceMount::parse_spec("proj").is_err());
        assert!(WorkspaceMount::parse_spec("proj:/a:rx").is_err());
        assert!(WorkspaceMount::parse_spec(":/a").is_err());
    }

    #[test]
    fn backing_serde_shape() {
        let b = WorkspaceBacking::HostDir("/srv/x".into());
        assert_eq!(
            serde_json::to_value(&b).unwrap(),
            serde_json::json!({"kind": "host_dir", "path": "/srv/x"})
        );
    }
}
