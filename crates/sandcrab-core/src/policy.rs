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
