//! Published ports: `127.0.0.1` listeners → `tunnel.open` over `v.sock` (design §9).
//!
//! One [`sandcrab_core::PortForwarder`] per `SandboxConfig.ports` entry, bound on
//! **`127.0.0.1:<ephemeral>`** once the guest is ready. Every accepted TCP connection opens a
//! fresh `v.sock` connection (the vsock port libkrun maps to a Unix socket in the run dir),
//! sends `tunnel.open{port}`, and splices raw bytes to guest `127.0.0.1:<port>`. It needs no
//! NIC and no privileges, so it works with network policy `disabled`. TSI port maps are never
//! used (they bind the host wildcard address).
//!
//! - A guest port nothing listens on yet is not a spawn error: each connection to it is
//!   refused by `crabd` (`-32004`) and the client sees the connection close.
//! - At most 64 tunnels per published port on the host, 64 per sandbox in `crabd`.
//! - The forwards stop on destroy, on drop of the sandbox, and as soon as the helper exits
//!   (crash watch), so a dead VM never leaves a listener behind.
//!
//! **Half-close (measured on HVF, libkrun 1.19.8):** a guest `SHUT_WR` reaches the host as EOF,
//! but a host `SHUT_WR` on the `v.sock` Unix socket breaks the stream (the guest's reply is
//! lost and the host never sees EOF). So the client's half-close is not forwarded
//! ([`ForwarderConfig::forward_half_close`] `= false`, like Firecracker): HTTP/1.0 clients that
//! half-close after the request get their reply, protocols that need the guest to see the
//! client's EOF (upload-until-EOF) do not work.

use std::sync::Arc;

use sandcrab_core::{Connector, ForwarderConfig, PortForwards, PortSpec, Result};

use crate::vm::{ExitRx, KrunConnector, until_exit};

/// Whether a client's `SHUT_WR` is forwarded into the tunnel (see the module docs).
pub const HOST_HALF_CLOSE: bool = false;

/// Check the port list before anything is created.
pub fn check(ports: &[PortSpec]) -> Result<()> {
    sandcrab_core::validate_ports(ports)
}

/// Bind the forwards for a ready sandbox; they stop by themselves when the helper exits.
pub(crate) async fn publish(
    connector: Arc<KrunConnector>,
    ports: &[PortSpec],
    exit: Option<ExitRx>,
) -> Result<PortForwards> {
    let until = exit.map(|rx| -> sandcrab_core::StopSignal {
        Box::pin(async move {
            let _ = until_exit(rx).await;
        })
    });
    let connector: Arc<dyn Connector> = connector;
    let config = ForwarderConfig {
        forward_half_close: HOST_HALF_CLOSE,
        ..ForwarderConfig::default()
    };
    PortForwards::bind(connector, ports, config, until).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_list_checks() {
        check(&[]).unwrap();
        check(&[PortSpec { guest: 8080 }, PortSpec { guest: 3000 }]).unwrap();
        assert!(check(&[PortSpec { guest: 0 }]).is_err());
        assert!(check(&[PortSpec { guest: 80 }, PortSpec { guest: 80 }]).is_err());
    }
}
