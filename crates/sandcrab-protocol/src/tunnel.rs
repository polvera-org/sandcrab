//! `tunnel.open`: TCP port forwarding to a guest loopback port over the `crabd` transport.
//!
//! microVM backends have no Docker-style published ports (Firecracker has no port publishing;
//! libkrun's TSI is disabled because it exposes host loopback). Instead the host listens on
//! `127.0.0.1` and, for every accepted TCP connection, opens a **new, dedicated** transport
//! connection to `crabd` (vsock, vsock-over-UDS or Unix socket) and sends one frame:
//!
//! ```text
//! -> {"jsonrpc":"2.0","id":1,"method":"tunnel.open","params":{"port":3000}}
//! <- {"jsonrpc":"2.0","id":1,"result":{"port":3000}}
//! ... then raw bytes in both directions, no more frames, until either side closes ...
//! ```
//!
//! `tunnel.open` must be the **first** frame of a connection. `crabd` connects to the guest's
//! own loopback (`127.0.0.1:<port>`, then `[::1]:<port>`) *before* acking, so a successful
//! ack means the byte stream is live. On error `crabd` answers with an error frame and closes
//! the connection. The guest target is always loopback: the params carry a port, never a
//! host, so a tunnel cannot be pointed anywhere else.
//!
//! The method is a connection-level verb, not a tool: it is not in [`crate::TOOL_NAMES`] and
//! not part of [`crate::Call`]. Sent on a connection that already carried a frame, `crabd`
//! answers `INVALID_REQUEST` and keeps the connection in frame mode.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ProtocolError, RequestId, RpcRequest,
    tools::{from_params, to_json},
};

/// Method that switches a fresh connection into a raw byte tunnel.
pub const METHOD_TUNNEL_OPEN: &str = "tunnel.open";

/// `tunnel.open` params.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TunnelOpenParams {
    /// TCP port on the guest's loopback interface (1..=65535).
    pub port: u16,
}

impl TunnelOpenParams {
    /// Params for `port`; fails on port `0`.
    pub fn new(port: u16) -> Result<Self, ProtocolError> {
        let p = Self { port };
        p.validate()?;
        Ok(p)
    }

    /// Port `0` is not a connectable port.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.port == 0 {
            return Err(ProtocolError::InvalidParams(
                "`port` must be between 1 and 65535".into(),
            ));
        }
        Ok(())
    }

    /// Wrap in a request envelope.
    #[must_use]
    pub fn to_request(&self, id: impl Into<RequestId>) -> RpcRequest {
        RpcRequest::new(id, METHOD_TUNNEL_OPEN, Some(to_json(self)))
    }

    /// Decode and validate from a `tunnel.open` request. Any other method is
    /// [`ProtocolError::MethodNotFound`].
    pub fn from_request(req: &RpcRequest) -> Result<Self, ProtocolError> {
        if req.method != METHOD_TUNNEL_OPEN {
            return Err(ProtocolError::MethodNotFound(req.method.clone()));
        }
        let p: Self = from_params(req.params.clone().unwrap_or(Value::Null))?;
        p.validate()?;
        Ok(p)
    }
}

/// `tunnel.open` result: the guest port the tunnel is connected to. After this frame the
/// connection carries raw bytes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct TunnelOpenAck {
    /// Guest loopback port the stream is connected to.
    pub port: u16,
}

impl TunnelOpenAck {
    #[must_use]
    pub fn to_value(&self) -> Value {
        to_json(self)
    }

    /// Decode a `tunnel.open` `result` value (unknown fields are accepted).
    pub fn from_value(value: Value) -> Result<Self, ProtocolError> {
        serde_json::from_value(value)
            .map_err(|e| ProtocolError::Internal(format!("malformed `tunnel.open` result: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Call, RpcMessage, encode_frame};
    use serde_json::json;

    #[test]
    fn request_round_trip() {
        let p = TunnelOpenParams::new(8080).unwrap();
        let req = p.to_request(7);
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({"jsonrpc":"2.0","id":7,"method":"tunnel.open","params":{"port":8080}})
        );
        let frame = encode_frame(&req).unwrap();
        let RpcMessage::Request(back) = RpcMessage::from_slice(&frame[4..]).unwrap() else {
            panic!("request")
        };
        assert_eq!(TunnelOpenParams::from_request(&back).unwrap(), p);
    }

    #[test]
    fn rejects_port_zero_and_out_of_range() {
        assert!(matches!(
            TunnelOpenParams::new(0),
            Err(ProtocolError::InvalidParams(_))
        ));
        for params in [
            json!({"port": 0}),
            json!({"port": 65536}),
            json!({"port": -1}),
            json!({"port": "80"}),
            json!({}),
            json!(null),
            json!([80]),
        ] {
            let req = RpcRequest::new(1, METHOD_TUNNEL_OPEN, Some(params.clone()));
            assert!(
                matches!(
                    TunnelOpenParams::from_request(&req),
                    Err(ProtocolError::InvalidParams(_))
                ),
                "{params}"
            );
        }
    }

    #[test]
    fn target_is_port_only() {
        // No host field: a tunnel can only ever reach guest loopback.
        let req = RpcRequest::new(
            1,
            METHOD_TUNNEL_OPEN,
            Some(json!({"port": 80, "host": "10.0.0.1"})),
        );
        let err = TunnelOpenParams::from_request(&req).unwrap_err();
        assert!(
            matches!(err, ProtocolError::InvalidParams(ref m) if m.contains("host")),
            "{err}"
        );
    }

    #[test]
    fn other_methods_are_not_tunnels() {
        let req = RpcRequest::new(1, "bash", Some(json!({"port": 80})));
        assert!(matches!(
            TunnelOpenParams::from_request(&req),
            Err(ProtocolError::MethodNotFound(_))
        ));
    }

    #[test]
    fn not_a_tool_call() {
        assert!(!crate::TOOL_NAMES.contains(&METHOD_TUNNEL_OPEN));
        assert!(matches!(
            Call::from_method_params(METHOD_TUNNEL_OPEN, json!({"port": 80})),
            Err(ProtocolError::MethodNotFound(_))
        ));
    }

    #[test]
    fn ack_accepts_unknown_fields() {
        let ack = TunnelOpenAck::from_value(json!({"port": 5, "extra": true})).unwrap();
        assert_eq!(ack, TunnelOpenAck { port: 5 });
        assert_eq!(ack.to_value(), json!({"port": 5}));
        assert!(matches!(
            TunnelOpenAck::from_value(json!({})),
            Err(ProtocolError::Internal(_))
        ));
    }
}
