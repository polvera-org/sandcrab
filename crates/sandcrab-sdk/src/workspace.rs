//! Host-side workspace and template access.

use std::path::{Path, PathBuf};

use sandcrab_core::{
    FileEntry, Result, Template, TemplateRegistry, Workspace, WorkspaceRef, WorkspaceRegistry,
};

/// Host-side operations on persistent workspaces. Cheap to clone.
///
/// Usable without any backend ([`WorkspaceClient::open`]), e.g. to stage files between agent
/// turns. A workspace mounted read-write by a running sandbox refuses host mutation
/// ([`sandcrab_core::SandboxError::WorkspaceBusy`]) — leases are tracked per process.
#[derive(Debug, Clone)]
pub struct WorkspaceClient {
    registry: WorkspaceRegistry,
}

impl WorkspaceClient {
    /// Open the registry at `state_dir` (created `0700` if missing).
    pub fn open(state_dir: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::from_registry(WorkspaceRegistry::open(state_dir)?))
    }

    /// Open the registry at `$SANDCRAB_HOME`, else `~/.sandcrab`.
    pub fn open_default() -> Result<Self> {
        Ok(Self::from_registry(WorkspaceRegistry::open_default()?))
    }

    #[must_use]
    pub fn from_registry(registry: WorkspaceRegistry) -> Self {
        Self { registry }
    }

    #[must_use]
    pub fn registry(&self) -> &WorkspaceRegistry {
        &self.registry
    }

    /// Canonical state root.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        self.registry.state_root()
    }

    /// Create a Sandcrab-managed workspace (`<state>/workspaces/<id>/data`).
    pub fn create(&self, name: &str) -> Result<Workspace> {
        self.registry.create(name)
    }

    /// Register an existing host directory (never deleted by Sandcrab).
    pub fn register_host_dir(&self, name: &str, path: impl AsRef<Path>) -> Result<Workspace> {
        self.registry.register_host_dir(name, path)
    }

    /// All workspaces, sorted by name.
    pub fn list(&self) -> Result<Vec<Workspace>> {
        self.registry.list()
    }

    /// Look up by name or id.
    pub fn get(&self, workspace: impl Into<WorkspaceRef>) -> Result<Workspace> {
        self.registry.get(&workspace.into())
    }

    /// Write a file (parents created). `rel_path` must stay inside the workspace.
    pub fn put_file(
        &self,
        workspace: impl Into<WorkspaceRef>,
        rel_path: impl AsRef<Path>,
        bytes: impl AsRef<[u8]>,
    ) -> Result<()> {
        self.registry
            .put_file(&workspace.into(), rel_path, bytes.as_ref())
    }

    /// Read a whole file, up to [`sandcrab_core::DEFAULT_READ_LIMIT`] bytes
    /// ([`sandcrab_core::SandboxError::TooLarge`] beyond).
    pub fn read_file(
        &self,
        workspace: impl Into<WorkspaceRef>,
        rel_path: impl AsRef<Path>,
    ) -> Result<Vec<u8>> {
        self.registry.read_file(&workspace.into(), rel_path)
    }

    /// [`Self::read_file`] with an explicit size limit in bytes.
    pub fn read_file_limited(
        &self,
        workspace: impl Into<WorkspaceRef>,
        rel_path: impl AsRef<Path>,
        limit: u64,
    ) -> Result<Vec<u8>> {
        self.registry
            .read_file_limited(&workspace.into(), rel_path, limit)
    }

    /// Open a file for streaming (no size limit): `(reader, size at open)`.
    pub fn open_file(
        &self,
        workspace: impl Into<WorkspaceRef>,
        rel_path: impl AsRef<Path>,
    ) -> Result<(std::fs::File, u64)> {
        self.registry.open_file(&workspace.into(), rel_path)
    }

    /// Delete a file, symlink or directory (recursively).
    pub fn delete_file(
        &self,
        workspace: impl Into<WorkspaceRef>,
        rel_path: impl AsRef<Path>,
    ) -> Result<()> {
        self.registry.delete_file(&workspace.into(), rel_path)
    }

    /// List one directory (non-recursive); `""` or `"."` is the workspace root.
    pub fn list_files(
        &self,
        workspace: impl Into<WorkspaceRef>,
        rel_dir: impl AsRef<Path>,
    ) -> Result<Vec<FileEntry>> {
        self.registry.list_files(&workspace.into(), rel_dir)
    }

    /// Unregister. `delete_data` deletes a managed workspace's data (never a registered
    /// host directory).
    pub fn remove(
        &self,
        workspace: impl Into<WorkspaceRef>,
        delete_data: bool,
    ) -> Result<Workspace> {
        self.registry.remove(&workspace.into(), delete_data)
    }
}

/// Read access to loaded templates. Cheap to clone.
#[derive(Debug, Clone)]
pub struct TemplateClient {
    registry: TemplateRegistry,
}

impl TemplateClient {
    /// Load `<dir>/<name>/sandcrab.toml` from `dirs` (first dir wins on a name clash).
    pub fn load<P: AsRef<Path>>(dirs: &[P]) -> Result<Self> {
        Ok(Self::from_registry(TemplateRegistry::load(dirs)?))
    }

    #[must_use]
    pub fn from_registry(registry: TemplateRegistry) -> Self {
        Self { registry }
    }

    #[must_use]
    pub fn registry(&self) -> &TemplateRegistry {
        &self.registry
    }

    /// All templates, sorted by name.
    #[must_use]
    pub fn list(&self) -> Vec<Template> {
        self.registry.list().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Result<Template> {
        self.registry.get(name).cloned()
    }

    /// Directory a template was loaded from.
    pub fn dir(&self, name: &str) -> Result<PathBuf> {
        Ok(self.registry.get(name)?.dir.clone())
    }
}

#[cfg(test)]
mod tests {
    use sandcrab_core::{FileKind, SandboxError};

    use super::*;

    #[test]
    fn workspace_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = WorkspaceClient::open(tmp.path().join("state")).unwrap();
        let created = ws.create("demo").unwrap();
        assert_eq!(ws.get("demo").unwrap().id, created.id);
        assert_eq!(ws.get(created.id).unwrap().name, "demo");

        ws.put_file("demo", "src/a.txt", "hello").unwrap();
        assert_eq!(ws.read_file("demo", "src/a.txt").unwrap(), b"hello");
        let root = ws.list_files("demo", "").unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].kind, FileKind::Dir);
        let files = ws.list_files("demo", "src").unwrap();
        assert_eq!(files[0].path, PathBuf::from("src/a.txt"));
        assert_eq!(files[0].size, 5);

        let err = ws.put_file("demo", "../escape", "x").unwrap_err();
        assert!(
            matches!(
                err,
                SandboxError::PathEscape(_) | SandboxError::InvalidConfig(_)
            ),
            "{err}"
        );

        ws.delete_file("demo", "src").unwrap();
        assert!(ws.list_files("demo", "").unwrap().is_empty());

        let host = tmp.path().join("host");
        std::fs::create_dir(&host).unwrap();
        ws.register_host_dir("hosted", &host).unwrap();
        assert_eq!(ws.list().unwrap().len(), 2);
        ws.remove("hosted", true).unwrap();
        assert!(host.is_dir(), "host dir is never deleted");
        ws.remove("demo", true).unwrap();
        assert!(ws.list().unwrap().is_empty());
    }
}
