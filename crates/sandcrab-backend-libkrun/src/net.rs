//! Network policy → vsock / TSI / NIC decisions (design §9).
//!
//! TSI is **never** enabled implicitly: it hands the guest the host's loopback and
//! auto-publishes guest listeners on the host wildcard address. The helper always sets an
//! empty port map (there is no port-map field in [`crate::HelperConfig`]).
//!
//! | `network.policy` | outcome |
//! |---|---|
//! | `disabled` | [`HelperNetwork::Disabled`]: TSI off (`disable_implicit_vsock` + `add_vsock(0)`), no NIC |
//! | `unrestricted` | [`HelperNetwork::Userspace`] when the pinned netstack is installed (passt / gvproxy behind the helper's egress filter, [`crate::netstack`]); otherwise [`HelperNetwork::TsiUnrestricted`] only with the deprecated [`LibkrunConfig::allow_tsi_unrestricted`](crate::LibkrunConfig::allow_tsi_unrestricted) (**the guest can reach host loopback services**); otherwise `PolicyUnenforceable` |
//! | `egress_restricted` | `PolicyUnenforceable` (no egress proxy before Phase 3) |
//!
//! An installed netstack wins over the deprecated TSI opt-in: the opt-in is then ignored with
//! an error-level log on every spawn, because the safe path exists and TSI would hand the guest
//! the host's loopback. The opt-in only takes effect where no netstack is installed. A missing
//! netstack never falls back to TSI without it.
//!
//! A domain allow-list is only meaningful with `egress_restricted`; on any other policy it is
//! rejected rather than ignored. Published ports do not depend on the policy: they go through
//! `tunnel.open` over vsock ([`crate::ports`]); neither TSI nor netstack port mapping is used.

use sandcrab_core::{NetworkConfig, NetworkPolicy, Result, SandboxError};

use crate::helper_proto::HelperNetwork;
use crate::netstack::NetstackKind;

/// What the operator explicitly allowed and what is installed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetOptions {
    /// `LibkrunConfig::allow_tsi_unrestricted` (deprecated).
    pub allow_tsi_unrestricted: bool,
    /// The pinned userspace netstack, if installed (`KrunLibs::netstack`).
    pub userspace: Option<NetstackKind>,
}

/// Map a network policy to the helper's network mode, or explain why it cannot be enforced.
pub fn plan(network: &NetworkConfig, opts: NetOptions) -> Result<HelperNetwork> {
    if network.policy != NetworkPolicy::EgressRestricted && !network.allow_domains.is_empty() {
        return Err(SandboxError::InvalidConfig(format!(
            "network policy `{}` does not take a domain allow-list ({} domain(s) given); \
             the libkrun backend never ignores network settings",
            network.policy,
            network.allow_domains.len()
        )));
    }
    match network.policy {
        NetworkPolicy::Disabled => Ok(HelperNetwork::Disabled),
        NetworkPolicy::Unrestricted if opts.userspace.is_some() => {
            if opts.allow_tsi_unrestricted {
                tracing::error!(
                    netstack = opts.userspace.map(NetstackKind::as_str),
                    "allow_tsi_unrestricted (--libkrun-tsi) is deprecated and IGNORED: the pinned \
                     userspace netstack is installed and is used instead (TSI would let the guest \
                     reach this host's loopback); drop the option"
                );
            }
            Ok(HelperNetwork::Userspace)
        }
        NetworkPolicy::Unrestricted if opts.allow_tsi_unrestricted => {
            Ok(HelperNetwork::TsiUnrestricted)
        }
        NetworkPolicy::Unrestricted => Err(SandboxError::PolicyUnenforceable(format!(
            "network policy `unrestricted` on libkrun needs the pinned userspace network stack \
             ({}), which is not installed: run `sandcrab libkrun install` (or \
             scripts/fetch-libkrun.sh). TSI is never used implicitly (it lets the guest reach \
             services bound to the host's loopback)",
            NetstackKind::for_host().map_or("passt / gvproxy", NetstackKind::as_str)
        ))),
        NetworkPolicy::EgressRestricted => Err(SandboxError::PolicyUnenforceable(format!(
            "network policy `egress_restricted` (allow-list of {} domain(s)) cannot be enforced \
             on libkrun: there is no egress proxy before Phase 3; use `disabled` or \
             `unrestricted`",
            network.allow_domains.len()
        ))),
    }
}

/// Whether [`plan`] accepts `network` (for `RuntimeBackend::enforces_network_policy`).
#[must_use]
pub fn enforces(network: &NetworkConfig, opts: NetOptions) -> bool {
    plan(network, opts).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFF: NetOptions = NetOptions {
        allow_tsi_unrestricted: false,
        userspace: None,
    };
    const TSI: NetOptions = NetOptions {
        allow_tsi_unrestricted: true,
        userspace: None,
    };
    const NETSTACK: NetOptions = NetOptions {
        allow_tsi_unrestricted: false,
        userspace: Some(NetstackKind::Passt),
    };
    const BOTH: NetOptions = NetOptions {
        allow_tsi_unrestricted: true,
        userspace: Some(NetstackKind::Gvproxy),
    };

    #[test]
    fn disabled_is_tsi_off_and_nic_free() {
        for opts in [OFF, TSI, NETSTACK, BOTH] {
            assert_eq!(
                plan(&NetworkConfig::disabled(), opts).unwrap(),
                HelperNetwork::Disabled,
                "neither the TSI opt-in nor the netstack changes `disabled`"
            );
        }
    }

    #[test]
    fn unrestricted_uses_the_netstack_and_never_falls_back_to_tsi() {
        assert_eq!(
            plan(&NetworkConfig::unrestricted(), NETSTACK).unwrap(),
            HelperNetwork::Userspace
        );
        assert!(enforces(&NetworkConfig::unrestricted(), NETSTACK));

        let err = plan(&NetworkConfig::unrestricted(), OFF).unwrap_err();
        assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains("libkrun install") && msg.contains("loopback"),
            "{msg}"
        );
        assert!(!enforces(&NetworkConfig::unrestricted(), OFF));
    }

    #[test]
    fn tsi_opt_in_only_applies_without_a_netstack() {
        assert_eq!(
            plan(&NetworkConfig::unrestricted(), TSI).unwrap(),
            HelperNetwork::TsiUnrestricted
        );
        // The installed netstack wins: the deprecated opt-in is ignored (logged), never TSI.
        assert_eq!(
            plan(&NetworkConfig::unrestricted(), BOTH).unwrap(),
            HelperNetwork::Userspace
        );
    }

    #[test]
    fn egress_allow_list_is_unenforceable() {
        for opts in [OFF, TSI, NETSTACK, BOTH] {
            let net = NetworkConfig::egress_restricted(["example.com"]);
            let err = plan(&net, opts).unwrap_err();
            assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");
            assert!(err.to_string().contains("egress_restricted"), "{err}");
            assert!(!enforces(&net, opts));
        }
    }

    #[test]
    fn stray_allow_list_is_rejected_not_ignored() {
        for policy in [NetworkPolicy::Disabled, NetworkPolicy::Unrestricted] {
            let net = NetworkConfig {
                policy,
                allow_domains: vec!["example.com".into()],
            };
            let err = plan(&net, BOTH).unwrap_err();
            assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
        }
    }
}
