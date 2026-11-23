use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Result, SandboxError};

/// Manifest parsed from `templates/<name>/sandcrab.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateManifest {
    pub template: TemplateMeta,
    #[serde(default)]
    pub resources: ResourceLimits,
    #[serde(default)]
    pub workspace: WorkspaceDefaults,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub tools: ToolConfig,
}

impl TemplateManifest {
    pub fn from_toml_str(input: &str) -> Result<Self> {
        toml::from_str(input).map_err(|e| SandboxError::Template(e.to_string()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateMeta {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Optional OCI image tag or SHA-256 pinned digest.
    #[serde(default)]
    pub image: Option<String>,
    /// Optional expected SHA-256 digest of built rootfs/image artifact.
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    #[serde(default = "default_vcpu")]
    pub vcpu: u32,
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u64,
    #[serde(default = "default_disk_mb")]
    pub disk_mb: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            vcpu: default_vcpu(),
            memory_mb: default_memory_mb(),
            disk_mb: default_disk_mb(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDefaults {
    #[serde(default = "default_workdir")]
    pub workdir: PathBuf,
}

impl Default for WorkspaceDefaults {
    fn default() -> Self {
        Self {
            workdir: default_workdir(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    Disabled,
    #[default]
    EgressRestricted,
    Unrestricted,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkConfig {
    #[serde(default)]
    pub default_policy: NetworkPolicy,
    #[serde(default)]
    pub allow_domains: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolConfig {
    #[serde(default)]
    pub tiers: Vec<String>,
}

const fn default_vcpu() -> u32 {
    2
}

const fn default_memory_mb() -> u64 {
    1024
}

const fn default_disk_mb() -> u64 {
    4096
}

fn default_workdir() -> PathBuf {
    PathBuf::from("/workspace")
}
