use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    MountMode, NetworkConfig, PortSpec, ResourceOverrides, Result, SandboxError, SecretSpec,
    UserSpec, WorkspaceMount, WorkspaceRef,
};

/// Unique identifier for an ephemeral sandbox instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SandboxId(pub Uuid);

impl SandboxId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Display for SandboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Default for SandboxId {
    fn default() -> Self {
        Self::new()
    }
}

/// Default sandbox time-to-live: 30 minutes, then the manager's reaper destroys it.
pub const DEFAULT_TTL_SECS: u64 = 30 * 60;

#[allow(clippy::unnecessary_wraps)] // serde default fn must return the field type
fn default_ttl() -> Option<u64> {
    Some(DEFAULT_TTL_SECS)
}

/// Caller-facing configuration for spawning an ephemeral sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Template name (see `TemplateRegistry`).
    pub template: String,
    #[serde(default)]
    pub mounts: Vec<WorkspaceMount>,
    /// Working directory for tools. Default: first rw mount, else the template workdir.
    #[serde(default)]
    pub default_cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Network policy. `None` → the template's `[network]` → `disabled`. A backend that
    /// cannot enforce the effective policy fails the spawn.
    #[serde(default)]
    pub network: Option<NetworkConfig>,
    /// Reserved for Phase 3; must be empty in V1 (spawn fails otherwise).
    #[serde(default)]
    pub secrets: Vec<SecretSpec>,
    /// Guest ports to publish on host loopback.
    #[serde(default)]
    pub ports: Vec<PortSpec>,
    /// Guest user override (`uid:gid`); default: template user, else the backend default.
    #[serde(default)]
    pub user: Option<UserSpec>,
    #[serde(default)]
    pub resources: ResourceOverrides,
    /// Time-to-live in seconds; `None` = no TTL (destroyed only explicitly / on shutdown).
    #[serde(default = "default_ttl")]
    pub ttl_secs: Option<u64>,
    /// Refuse to spawn unless the backend is hardware isolated.
    #[serde(default)]
    pub require_hardware_isolation: bool,
}

impl SandboxConfig {
    #[must_use]
    pub fn from_template(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
            mounts: Vec::new(),
            default_cwd: None,
            env: BTreeMap::new(),
            network: None,
            secrets: Vec::new(),
            ports: Vec::new(),
            user: None,
            resources: ResourceOverrides::default(),
            ttl_secs: default_ttl(),
            require_hardware_isolation: false,
        }
    }

    #[must_use]
    pub fn mount(
        mut self,
        workspace: impl Into<WorkspaceRef>,
        guest_path: impl Into<PathBuf>,
        mode: MountMode,
    ) -> Self {
        self.mounts.push(WorkspaceMount {
            workspace: workspace.into(),
            guest_path: guest_path.into(),
            mode,
        });
        self
    }

    #[must_use]
    pub fn default_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.default_cwd = Some(cwd.into());
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub fn network(mut self, network: NetworkConfig) -> Self {
        self.network = Some(network);
        self
    }

    #[must_use]
    pub fn port(mut self, guest: u16) -> Self {
        self.ports.push(PortSpec { guest });
        self
    }

    #[must_use]
    pub fn user(mut self, user: UserSpec) -> Self {
        self.user = Some(user);
        self
    }

    #[must_use]
    pub fn resources(mut self, resources: ResourceOverrides) -> Self {
        self.resources = resources;
        self
    }

    #[must_use]
    pub fn ttl(mut self, ttl: Option<Duration>) -> Self {
        self.ttl_secs = ttl.map(|d| d.as_secs().max(1));
        self
    }

    #[must_use]
    pub fn require_hardware_isolation(mut self, required: bool) -> Self {
        self.require_hardware_isolation = required;
        self
    }

    #[must_use]
    pub fn ttl_duration(&self) -> Option<Duration> {
        self.ttl_secs.map(Duration::from_secs)
    }

    /// Static checks that need no registry or backend (mounts are checked by
    /// [`crate::resolve_mounts`]).
    pub fn validate(&self) -> Result<()> {
        for key in self.env.keys() {
            let ok = !key.is_empty()
                && !key.as_bytes()[0].is_ascii_digit()
                && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            if !ok {
                return Err(SandboxError::InvalidConfig(format!(
                    "env var name `{key}` must match [A-Za-z_][A-Za-z0-9_]*"
                )));
            }
        }
        if let Some((k, _)) = self.env.iter().find(|(_, v)| v.contains('\0')) {
            return Err(SandboxError::InvalidConfig(format!(
                "env var `{k}` contains a NUL byte"
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for p in &self.ports {
            if p.guest == 0 {
                return Err(SandboxError::InvalidConfig(
                    "guest port 0 is invalid".into(),
                ));
            }
            if !seen.insert(p.guest) {
                return Err(SandboxError::InvalidConfig(format!(
                    "guest port {} is listed twice",
                    p.guest
                )));
            }
        }
        if self.ttl_secs == Some(0) {
            return Err(SandboxError::InvalidConfig(
                "ttl_secs must be positive (use None for no TTL)".into(),
            ));
        }
        if let Some(net) = &self.network {
            net.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkPolicy;

    #[test]
    fn builder_and_defaults() {
        let c = SandboxConfig::from_template("base")
            .mount("proj", "/workspace", MountMode::ReadWrite)
            .env("FOO", "1")
            .port(3000)
            .ttl(Some(Duration::from_secs(60)));
        c.validate().unwrap();
        assert_eq!(c.network, None);
        assert_eq!(c.ttl_duration(), Some(Duration::from_secs(60)));

        let parsed: SandboxConfig = serde_json::from_str(r#"{"template":"base"}"#).unwrap();
        assert_eq!(parsed, SandboxConfig::from_template("base"));
        assert_eq!(parsed.ttl_secs, Some(DEFAULT_TTL_SECS));
    }

    #[test]
    fn validation() {
        let base = SandboxConfig::from_template("base");
        for bad in [
            base.clone().env("1A", "x"),
            base.clone().env("A-B", "x"),
            base.clone().env("", "x"),
            base.clone().env("A", "x\0y"),
            base.clone().port(0),
            base.clone().port(80).port(80),
            SandboxConfig {
                ttl_secs: Some(0),
                ..base.clone()
            },
            base.clone().network(NetworkConfig {
                policy: NetworkPolicy::Disabled,
                allow_domains: vec!["a.com".into()],
            }),
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
