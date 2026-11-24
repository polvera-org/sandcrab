//! Network policy → vsock / TSI / port-map decisions (design §9).
//!
//! TSI is **never** enabled implicitly: it hands the guest the host's loopback and
//! auto-publishes guest listeners on the host wildcard address. The helper always sets an
//! empty port map (there is no port-map field in [`crate::HelperConfig`]).
//!
//! | `network.policy` | outcome |
//! |---|---|
//! | `disabled` | [`HelperNetwork::Disabled`]: TSI off (`disable_implicit_vsock` + `add_vsock(0)`), no NIC |
//! | `unrestricted` | `PolicyUnenforceable`, unless [`LibkrunConfig::allow_tsi_unrestricted`](crate::LibkrunConfig::allow_tsi_unrestricted) → [`HelperNetwork::TsiUnrestricted`] (**the guest can reach host loopback services**) |
//! | `egress_restricted` | `PolicyUnenforceable` (no egress proxy before Phase 3) |
//!
//! A domain allow-list is only meaningful with `egress_restricted`; on any other policy it is
//! rejected rather than ignored. Published ports are refused until the `tunnel.open` vsock
//! tunnel lands (`ports.rs`, flow step `lk-ports`); TSI port mapping is never used.

use sandcrab_core::{NetworkConfig, NetworkPolicy, PortSpec, Result, SandboxError};

use crate::helper_proto::HelperNetwork;

/// What the operator explicitly allowed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetOptions {
    /// `LibkrunConfig::allow_tsi_unrestricted`.
    pub allow_tsi_unrestricted: bool,
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
        NetworkPolicy::Unrestricted if opts.allow_tsi_unrestricted => {
            Ok(HelperNetwork::TsiUnrestricted)
        }
        NetworkPolicy::Unrestricted => Err(SandboxError::PolicyUnenforceable(
            "network policy `unrestricted` on libkrun needs a userspace network stack (Phase B). \
             The only option today is TSI, which lets the guest reach services bound to the \
             host's loopback; opt in explicitly with `LibkrunConfig::allow_tsi_unrestricted` \
             (CLI `--libkrun-tsi`) if that is acceptable"
                .into(),
        )),
        NetworkPolicy::EgressRestricted => Err(SandboxError::PolicyUnenforceable(format!(
            "network policy `egress_restricted` (allow-list of {} domain(s)) cannot be enforced \
             on libkrun: there is no egress proxy before Phase 3; use `disabled`",
            network.allow_domains.len()
        ))),
    }
}

/// Whether [`plan`] accepts `network` (for `RuntimeBackend::enforces_network_policy`).
#[must_use]
pub fn enforces(network: &NetworkConfig, opts: NetOptions) -> bool {
    plan(network, opts).is_ok()
}

/// Published ports need the `tunnel.open` vsock tunnel (flow step `lk-ports`); until then a
/// non-empty list is an explicit error, never silently dropped.
pub fn check_ports(ports: &[PortSpec]) -> Result<()> {
    if ports.is_empty() {
        return Ok(());
    }
    let list = ports
        .iter()
        .map(|p| p.guest.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(SandboxError::InvalidConfig(format!(
        "ports need tunnel support: the libkrun backend cannot publish guest port(s) {list} yet \
         (TSI port mapping is never used because it binds the host wildcard address)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFF: NetOptions = NetOptions {
        allow_tsi_unrestricted: false,
    };
    const TSI: NetOptions = NetOptions {
        allow_tsi_unrestricted: true,
    };

    #[test]
    fn disabled_is_tsi_off() {
        for opts in [OFF, TSI] {
            assert_eq!(
                plan(&NetworkConfig::disabled(), opts).unwrap(),
                HelperNetwork::Disabled,
                "the TSI opt-in never changes `disabled`"
            );
        }
    }

    #[test]
    fn unrestricted_needs_explicit_opt_in() {
        let err = plan(&NetworkConfig::unrestricted(), OFF).unwrap_err();
        assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains("loopback") && msg.contains("allow_tsi_unrestricted"),
            "{msg}"
        );
        assert!(!enforces(&NetworkConfig::unrestricted(), OFF));

        assert_eq!(
            plan(&NetworkConfig::unrestricted(), TSI).unwrap(),
            HelperNetwork::TsiUnrestricted
        );
        assert!(enforces(&NetworkConfig::unrestricted(), TSI));
    }

    #[test]
    fn egress_allow_list_is_unenforceable() {
        for opts in [OFF, TSI] {
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
            let err = plan(&net, TSI).unwrap_err();
            assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
        }
    }

    #[test]
    fn ports_are_refused_until_the_tunnel_lands() {
        check_ports(&[]).unwrap();
        let err = check_ports(&[PortSpec { guest: 8080 }, PortSpec { guest: 3000 }])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("tunnel") && err.contains("8080, 3000"),
            "{err}"
        );
    }
}
