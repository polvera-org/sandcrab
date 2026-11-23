//! Orphan reaping: remove Sandcrab-managed containers (and socket dirs) whose sandbox is
//! no longer tracked — e.g. after a crash or a cancelled spawn. Used by `doctor --fix`.
//!
//! Only containers carrying **both** `sandcrab.managed=true` and
//! `sandcrab.backend=docker-dev` are considered; unlabeled containers are never touched.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::Duration;

use bollard::query_parameters::ListContainersOptionsBuilder;
use sandcrab_core::{Result, SandboxId};
use uuid::Uuid;

use crate::DockerDevBackend;
use crate::backend::{backend_err, teardown};
use crate::spec::{BACKEND_LABEL_VALUE, LABEL_BACKEND, LABEL_MANAGED, LABEL_SANDBOX_ID};

/// A container removed by [`DockerDevBackend::reap_orphans`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapedContainer {
    pub container_id: String,
    pub name: Option<String>,
    /// From the `sandcrab.sandbox_id` label, when it parses.
    pub sandbox_id: Option<SandboxId>,
}

/// Outcome of [`DockerDevBackend::reap_orphans`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReapReport {
    pub containers: Vec<ReapedContainer>,
    /// Stale per-sandbox socket directories removed from `state_dir`.
    pub socket_dirs: Vec<std::path::PathBuf>,
}

fn is_managed(labels: &HashMap<String, String>) -> bool {
    labels.get(LABEL_MANAGED).map(String::as_str) == Some("true")
        && labels.get(LABEL_BACKEND).map(String::as_str) == Some(BACKEND_LABEL_VALUE)
}

impl DockerDevBackend {
    /// Remove every managed docker-dev container whose sandbox id is not in `tracked`, then
    /// every `state_dir/<uuid>` socket directory that is neither tracked nor backed by a
    /// remaining container.
    pub async fn reap_orphans(&self, tracked: &HashSet<SandboxId>) -> Result<ReapReport> {
        let filters = HashMap::from([(
            "label",
            vec![
                format!("{LABEL_MANAGED}=true"),
                format!("{LABEL_BACKEND}={BACKEND_LABEL_VALUE}"),
            ],
        )]);
        let containers = self
            .docker
            .list_containers(Some(
                ListContainersOptionsBuilder::new()
                    .all(true)
                    .filters(&filters)
                    .build(),
            ))
            .await
            .map_err(|e| backend_err("list containers", e))?;

        let mut report = ReapReport::default();
        let mut live = HashSet::new();
        for c in containers {
            let labels = c.labels.unwrap_or_default();
            // Belt and braces on top of the daemon-side filter.
            if !is_managed(&labels) {
                continue;
            }
            let Some(container_id) = c.id else { continue };
            let sandbox_id = labels
                .get(LABEL_SANDBOX_ID)
                .and_then(|s| Uuid::parse_str(s).ok())
                .map(SandboxId);
            if let Some(id) = sandbox_id
                && tracked.contains(&id)
            {
                live.insert(id);
                continue;
            }
            let dir = sandbox_id.map(|id| self.socket_dir(id));
            teardown(
                &self.docker,
                &container_id,
                dir.as_deref(),
                Duration::ZERO,
                None,
            )
            .await?;
            report.containers.push(ReapedContainer {
                container_id,
                name: c
                    .names
                    .and_then(|n| n.into_iter().next())
                    .map(|n| n.trim_start_matches('/').to_owned()),
                sandbox_id,
            });
        }

        let entries = match fs::read_dir(&self.config.state_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|n| Uuid::parse_str(n).ok())
                .map(SandboxId)
            else {
                continue;
            };
            if tracked.contains(&id) || live.contains(&id) {
                continue;
            }
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) && fs::remove_dir_all(&path).is_ok() {
                report.socket_dirs.push(path);
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_label_check() {
        let mut l = HashMap::new();
        assert!(!is_managed(&l));
        l.insert(LABEL_MANAGED.to_owned(), "true".to_owned());
        assert!(!is_managed(&l));
        l.insert(LABEL_BACKEND.to_owned(), "firecracker".to_owned());
        assert!(!is_managed(&l));
        l.insert(LABEL_BACKEND.to_owned(), BACKEND_LABEL_VALUE.to_owned());
        assert!(is_managed(&l));
        l.insert(LABEL_MANAGED.to_owned(), "false".to_owned());
        assert!(!is_managed(&l));
    }
}
