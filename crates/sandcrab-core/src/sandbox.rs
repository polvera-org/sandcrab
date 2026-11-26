use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ForwardSpec, HostMount, MAX_FORWARDS, MountMode, NetworkConfig, PortSpec, ResourceOverrides,
    Result, SandboxError, SecretSpec, UserSpec, WorkspaceMount, WorkspaceRef,
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

/// Labels one sandbox may carry.
pub const MAX_LABELS: usize = 16;
/// Longest label value, in bytes.
pub const MAX_LABEL_VALUE_LEN: usize = 256;

/// A label key is `[a-z0-9][a-z0-9._-]{0,62}` and does not start with `sandcrab.`.
pub fn validate_label_key(key: &str) -> Result<()> {
    let b = key.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
        && !key.starts_with("sandcrab.");
    if ok {
        Ok(())
    } else {
        Err(SandboxError::InvalidConfig(format!(
            "label key `{key}` must match [a-z0-9][a-z0-9._-]{{0,62}} and not start with \
             `sandcrab.`"
        )))
    }
}

/// Check a label set: at most [`MAX_LABELS`], valid keys, values of at most
/// [`MAX_LABEL_VALUE_LEN`] bytes.
pub fn validate_labels(labels: &BTreeMap<String, String>) -> Result<()> {
    if labels.len() > MAX_LABELS {
        return Err(SandboxError::InvalidConfig(format!(
            "{} labels given, at most {MAX_LABELS} are allowed",
            labels.len()
        )));
    }
    for (k, v) in labels {
        validate_label_key(k)?;
        if v.len() > MAX_LABEL_VALUE_LEN {
            return Err(SandboxError::InvalidConfig(format!(
                "label `{k}`: value is longer than {MAX_LABEL_VALUE_LEN} bytes"
            )));
        }
    }
    Ok(())
}

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
    /// Host directories mounted at the same path in the sandbox, allowed only beneath the
    /// manager's [`crate::MountPolicy`] roots. Separate from workspace `mounts`.
    #[serde(default)]
    pub host_mounts: Vec<HostMount>,
    /// Guest loopback ports forwarded to the host side (at most [`MAX_FORWARDS`]).
    #[serde(default)]
    pub forwards: Vec<ForwardSpec>,
    /// Kept by the manager in memory, set at create and immutable (see
    /// [`validate_labels`]).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Drop `noexec` from the guest's `/tmp`.
    #[serde(default)]
    pub tmp_exec: bool,
    /// Size of the guest's `/tmp` (counts against the sandbox's memory); `None`: the
    /// backend's default.
    #[serde(default)]
    pub tmp_size_mb: Option<u64>,
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
            host_mounts: Vec::new(),
            forwards: Vec::new(),
            labels: BTreeMap::new(),
            tmp_exec: false,
            tmp_size_mb: None,
        }
    }

    #[must_use]
    pub fn host_mount(mut self, path: impl Into<PathBuf>, mode: MountMode) -> Self {
        self.host_mounts.push(HostMount::new(path, mode));
        self
    }

    #[must_use]
    pub fn forward(mut self, guest_port: u16) -> Self {
        self.forwards.push(ForwardSpec::new(guest_port));
        self
    }

    #[must_use]
    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub fn tmp_exec(mut self, exec: bool) -> Self {
        self.tmp_exec = exec;
        self
    }

    #[must_use]
    pub fn tmp_size_mb(mut self, size_mb: u64) -> Self {
        self.tmp_size_mb = Some(size_mb);
        self
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
    pub fn network(mut self, network: impl Into<NetworkConfig>) -> Self {
        self.network = Some(network.into());
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
        validate_labels(&self.labels)?;
        if self.forwards.len() > MAX_FORWARDS {
            return Err(SandboxError::InvalidConfig(format!(
                "{} forwards given, at most {MAX_FORWARDS} are allowed",
                self.forwards.len()
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for f in &self.forwards {
            if f.guest_port == 0 || f.max_connections == Some(0) {
                return Err(SandboxError::InvalidConfig(format!(
                    "forward {}: guest_port and max_connections must be positive",
                    f.guest_port
                )));
            }
            if !seen.insert(f.guest_port) {
                return Err(SandboxError::InvalidConfig(format!(
                    "forward guest port {} is listed twice",
                    f.guest_port
                )));
            }
        }
        if self.tmp_size_mb == Some(0) {
            return Err(SandboxError::InvalidConfig(
                "tmp_size_mb must be positive".into(),
            ));
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
        let c = c.network(crate::NetworkPolicy::Unrestricted);
        assert_eq!(c.network, Some(NetworkConfig::unrestricted()));
        assert_eq!(c.ttl_duration(), Some(Duration::from_secs(60)));

        let parsed: SandboxConfig = serde_json::from_str(r#"{"template":"base"}"#).unwrap();
        assert_eq!(parsed, SandboxConfig::from_template("base"));
        assert_eq!(parsed.ttl_secs, Some(DEFAULT_TTL_SECS));
    }

    #[test]
    fn old_serialized_config_still_parses() {
        // Every field a pre-0.3 config carried, none of the new ones.
        let old = serde_json::json!({
            "template": "base",
            "mounts": [{"workspace": "proj", "guest_path": "/w", "mode": "read_write"}],
            "default_cwd": "/w",
            "env": {"A": "1"},
            "network": {"policy": "disabled"},
            "secrets": [],
            "ports": [{"guest": 3000}],
            "user": "1000:1000",
            "resources": {"vcpu": 2},
            "ttl_secs": 60,
            "require_hardware_isolation": false
        });
        let c: SandboxConfig = serde_json::from_value(old).unwrap();
        assert!(c.host_mounts.is_empty() && c.forwards.is_empty() && c.labels.is_empty());
        assert!(!c.tmp_exec && c.tmp_size_mb.is_none());
        assert_eq!(c.resources.pids_max, None);
        c.validate().unwrap();

        let new = SandboxConfig::from_template("base")
            .host_mount("/srv/data", MountMode::ReadOnly)
            .forward(5432)
            .label("run", "r1")
            .tmp_exec(true)
            .tmp_size_mb(512);
        let back: SandboxConfig =
            serde_json::from_str(&serde_json::to_string(&new).unwrap()).unwrap();
        assert_eq!(back, new);
    }

    #[test]
    fn label_validation() {
        for ok in ["a", "0", "run-id", "a.b_c-d", &"k".repeat(63), "sandcrabx"] {
            validate_label_key(ok).unwrap();
        }
        for bad in [
            "",
            "A",
            "-a",
            ".a",
            "_a",
            "a/b",
            "a b",
            "é",
            &"k".repeat(64),
            "sandcrab.owner",
        ] {
            assert!(validate_label_key(bad).is_err(), "{bad:?}");
        }
        let base = SandboxConfig::from_template("base");
        base.clone()
            .label("v", "x".repeat(MAX_LABEL_VALUE_LEN))
            .validate()
            .unwrap();
        assert!(
            base.clone()
                .label("v", "x".repeat(MAX_LABEL_VALUE_LEN + 1))
                .validate()
                .is_err()
        );
        let mut many = base.clone();
        for i in 0..MAX_LABELS {
            many = many.label(format!("k{i}"), "v");
        }
        many.validate().unwrap();
        assert!(many.label("one-more", "v").validate().is_err());
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
            base.clone().forward(0),
            base.clone().forward(80).forward(80),
            SandboxConfig {
                forwards: vec![ForwardSpec {
                    guest_port: 80,
                    max_connections: Some(0),
                }],
                ..base.clone()
            },
            SandboxConfig {
                forwards: (1..=17).map(ForwardSpec::new).collect(),
                ..base.clone()
            },
            base.clone().tmp_size_mb(0),
            base.clone().label("Bad", "x"),
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
