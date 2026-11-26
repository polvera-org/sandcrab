//! Security policy types shared by templates, sandbox configs and backends.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{Result, SandboxError};

/// Guest network policy.
///
/// The default is [`NetworkPolicy::Disabled`] (ADR 0001 §5, design §6): a backend that cannot
/// enforce the requested policy must fail at spawn, so the default has to be one every
/// backend can enforce. `EgressRestricted` is not enforceable by any V1 backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// No network interface besides loopback (and the host transport).
    #[default]
    Disabled,
    /// Egress only to `allow_domains`. Not enforceable in V1.
    EgressRestricted,
    /// Full outbound network access.
    Unrestricted,
}

impl NetworkPolicy {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::EgressRestricted => "egress_restricted",
            Self::Unrestricted => "unrestricted",
        }
    }
}

impl fmt::Display for NetworkPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for NetworkPolicy {
    type Err = SandboxError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "disabled" => Ok(Self::Disabled),
            "egress_restricted" => Ok(Self::EgressRestricted),
            "unrestricted" => Ok(Self::Unrestricted),
            other => Err(SandboxError::InvalidConfig(format!(
                "network policy must be disabled|egress_restricted|unrestricted, got `{other}`"
            ))),
        }
    }
}

/// Requested guest network configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NetworkConfig {
    #[serde(default)]
    pub policy: NetworkPolicy,
    /// Domains reachable under `egress_restricted` (`example.com` or `*.example.com`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_domains: Vec<String>,
}

impl From<NetworkPolicy> for NetworkConfig {
    /// A policy with no domain allow-list (`SandboxConfig::network(NetworkPolicy::Unrestricted)`).
    fn from(policy: NetworkPolicy) -> Self {
        Self {
            policy,
            allow_domains: Vec::new(),
        }
    }
}

impl NetworkConfig {
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn unrestricted() -> Self {
        Self {
            policy: NetworkPolicy::Unrestricted,
            allow_domains: Vec::new(),
        }
    }

    #[must_use]
    pub fn egress_restricted(domains: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            policy: NetworkPolicy::EgressRestricted,
            allow_domains: domains.into_iter().map(Into::into).collect(),
        }
    }

    /// `allow_domains` only makes sense with `egress_restricted`, and must be well-formed.
    pub fn validate(&self) -> Result<()> {
        if !self.allow_domains.is_empty() && self.policy != NetworkPolicy::EgressRestricted {
            return Err(SandboxError::InvalidConfig(format!(
                "allow_domains is only valid with network policy `egress_restricted`, not `{}`",
                self.policy
            )));
        }
        for d in &self.allow_domains {
            validate_domain(d)?;
        }
        Ok(())
    }
}

fn validate_domain(domain: &str) -> Result<()> {
    let host = domain.strip_prefix("*.").unwrap_or(domain);
    let ok = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    if ok {
        Ok(())
    } else {
        Err(SandboxError::InvalidConfig(format!(
            "invalid allow_domains entry `{domain}` (lowercase host name, optional `*.` prefix)"
        )))
    }
}

/// Numeric guest user, `uid:gid` in text form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UserSpec {
    pub uid: u32,
    pub gid: u32,
}

impl UserSpec {
    #[must_use]
    pub fn new(uid: u32, gid: u32) -> Self {
        Self { uid, gid }
    }
}

impl fmt::Display for UserSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.uid, self.gid)
    }
}

impl FromStr for UserSpec {
    type Err = SandboxError;

    fn from_str(s: &str) -> Result<Self> {
        let bad = || SandboxError::InvalidConfig(format!("user must be `uid:gid`, got `{s}`"));
        let (uid, gid) = s.split_once(':').ok_or_else(bad)?;
        Ok(Self {
            uid: uid.parse().map_err(|_| bad())?,
            gid: gid.parse().map_err(|_| bad())?,
        })
    }
}

impl TryFrom<String> for UserSpec {
    type Error = SandboxError;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<UserSpec> for String {
    fn from(u: UserSpec) -> Self {
        u.to_string()
    }
}

/// A secret to inject into the sandbox. **Reserved for Phase 3**: the type is stable so
/// configs can be written against it, but every V1 backend (and the manager) rejects a
/// non-empty `secrets` list rather than ignoring it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretSpec {
    /// Environment variable name the secret is exposed as inside the guest.
    pub name: String,
    pub source: SecretSource,
}

/// Where a [`SecretSpec`] value comes from on the host. Never serialized with its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SecretSource {
    /// Read from a host environment variable at spawn.
    HostEnv { var: String },
    /// Read from a host file at spawn.
    File { path: std::path::PathBuf },
}

/// A guest TCP port to publish on host loopback (dev-server previews).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortSpec {
    pub guest: u16,
}

/// A published port: guest port and the host loopback address it is reachable at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PublishedPort {
    pub guest: u16,
    pub host: std::net::SocketAddr,
}

/// Forwards one sandbox may declare.
pub const MAX_FORWARDS: usize = 16;

/// A guest loopback port whose connections are forwarded to a target the host side chooses
/// (design §4). The sandbox keeps its network policy; nothing is published on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ForwardSpec {
    pub guest_port: u16,
    /// Connections open at once through this forward. `None`: the guest's default (64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
}

impl ForwardSpec {
    #[must_use]
    pub fn new(guest_port: u16) -> Self {
        Self {
            guest_port,
            max_connections: None,
        }
    }
}

/// Per-sandbox overrides of the template's [`crate::ResourceLimits`], plus the process limits
/// that have no template default (`None`: the backend's default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResourceOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_mb: Option<u64>,
    /// Processes in the whole sandbox (a pids cgroup limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids_max: Option<u32>,
    /// `RLIMIT_NOFILE` of the sandbox's processes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nofile: Option<u64>,
}

impl ResourceOverrides {
    /// Whether a process limit (`pids_max`, `nofile`) is asked for.
    #[must_use]
    pub fn has_process_limits(&self) -> bool {
        self.pids_max.is_some() || self.nofile.is_some()
    }

    /// Apply onto the template limits and validate the result.
    pub fn apply(&self, base: &crate::ResourceLimits) -> Result<crate::ResourceLimits> {
        if self.pids_max == Some(0) || self.nofile == Some(0) {
            return Err(SandboxError::InvalidConfig(
                "pids_max and nofile must be positive".into(),
            ));
        }
        let merged = crate::ResourceLimits {
            vcpu: self.vcpu.unwrap_or(base.vcpu),
            memory_mb: self.memory_mb.unwrap_or(base.memory_mb),
            disk_mb: self.disk_mb.unwrap_or(base.disk_mb),
        };
        merged.validate()?;
        Ok(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_default_is_disabled() {
        assert_eq!(NetworkPolicy::default(), NetworkPolicy::Disabled);
        assert_eq!(NetworkConfig::default().policy, NetworkPolicy::Disabled);
        let parsed: NetworkConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, NetworkConfig::disabled());
    }

    #[test]
    fn network_policy_strings() {
        for p in [
            NetworkPolicy::Disabled,
            NetworkPolicy::EgressRestricted,
            NetworkPolicy::Unrestricted,
        ] {
            assert_eq!(p.as_str().parse::<NetworkPolicy>().unwrap(), p);
            assert_eq!(
                serde_json::to_value(p).unwrap(),
                serde_json::json!(p.as_str())
            );
        }
        assert!("open".parse::<NetworkPolicy>().is_err());
    }

    #[test]
    fn network_validation() {
        NetworkConfig::egress_restricted(["pypi.org", "*.github.com"])
            .validate()
            .unwrap();
        assert!(
            NetworkConfig {
                policy: NetworkPolicy::Unrestricted,
                allow_domains: vec!["a.com".into()],
            }
            .validate()
            .is_err()
        );
        for bad in ["", "*.", "A.com", "a..com", "-a.com", "a.com/x", "*.*.a"] {
            assert!(
                NetworkConfig::egress_restricted([bad]).validate().is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn resource_overrides() {
        let base = crate::ResourceLimits::default();
        let merged = ResourceOverrides {
            vcpu: Some(4),
            ..Default::default()
        }
        .apply(&base)
        .unwrap();
        assert_eq!(merged.vcpu, 4);
        assert_eq!(merged.memory_mb, base.memory_mb);
        for bad in [
            ResourceOverrides {
                memory_mb: Some(1),
                ..Default::default()
            },
            ResourceOverrides {
                pids_max: Some(0),
                ..Default::default()
            },
            ResourceOverrides {
                nofile: Some(0),
                ..Default::default()
            },
        ] {
            assert!(bad.apply(&base).is_err(), "{bad:?}");
        }
        let limits = ResourceOverrides {
            pids_max: Some(64),
            nofile: Some(4096),
            ..Default::default()
        };
        assert!(limits.has_process_limits());
        limits.apply(&base).unwrap();
        assert!(!ResourceOverrides::default().has_process_limits());
    }

    #[test]
    fn secret_spec_shape() {
        let s = SecretSpec {
            name: "API_KEY".into(),
            source: SecretSource::HostEnv {
                var: "MY_KEY".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::json!({"name": "API_KEY", "source": {"kind": "host_env", "var": "MY_KEY"}})
        );
    }

    #[test]
    fn user_spec() {
        let u: UserSpec = "1000:100".parse().unwrap();
        assert_eq!(u, UserSpec::new(1000, 100));
        assert_eq!(
            serde_json::to_value(u).unwrap(),
            serde_json::json!("1000:100")
        );
        for bad in ["1000", "a:b", "1:2:3", "-1:0", ""] {
            assert!(bad.parse::<UserSpec>().is_err(), "{bad}");
        }
    }
}
