//! Forwards: a loopback port inside the sandbox whose connections the **host** routes to a
//! target it chose. The sandbox keeps network `disabled`; every connection is still opened by
//! the host.
//!
//! `crabd serve --forward-port <p>` (or `init.configure` `forward_ports`) binds
//! `127.0.0.1:<p>` (and `[::1]:<p>`) before `hello` answers. Ports cannot be added later.
//!
//! ```text
//! program in the sandbox -- connect 127.0.0.1:P --> crabd
//! (listen connection)  -> {"jsonrpc":"2.0","id":1,"method":"forward.listen","params":{}}
//!                      <- {"jsonrpc":"2.0","id":1,"result":{"ports":[P]}}
//!                      <- {"jsonrpc":"2.0","method":"forward.incoming","params":{"conn_id":7,"port":P}}
//! (new connection)     -> {"jsonrpc":"2.0","id":1,"method":"forward.accept","params":{"conn_id":7}}
//!                      <- {"jsonrpc":"2.0","id":1,"result":{"port":P}}
//!                      ... DATA / EOF records both ways until either side closes ...
//! ```
//!
//! When the host cannot reach its target it sends a `forward.reject` notification on the
//! listen connection. `forward.listen` and `forward.accept` are connection-level verbs (first
//! frame of a fresh connection); `forward.stats` is a method on a tool connection. None of them
//! is in [`crate::TOOL_NAMES`].
//!
//! Every refusal closes the program's connection (it looks like a flaky network from inside)
//! and adds one to a counter of [`ForwardPortStats`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    JsonRpcVersion, ProtocolError, RequestId, RpcNotification, RpcRequest,
    tools::{from_params, to_json},
};

/// Connection-level verb: this connection receives `forward.incoming` notifications.
pub const METHOD_FORWARD_LISTEN: &str = "forward.listen";
/// Notification from `crabd` on the listen connection: a program connected to a forward port.
pub const METHOD_FORWARD_INCOMING: &str = "forward.incoming";
/// Notification from the host on the listen connection: the target could not be reached.
pub const METHOD_FORWARD_REJECT: &str = "forward.reject";
/// Connection-level verb: this connection carries one announced connection as records.
pub const METHOD_FORWARD_ACCEPT: &str = "forward.accept";
/// Method on a tool connection: per-port counters since start.
pub const METHOD_FORWARD_STATS: &str = "forward.stats";

/// Forwards per sandbox (validated by `sandcrab-core`).
pub const MAX_FORWARDS: usize = 16;
/// Default connections at once per forward.
pub const DEFAULT_CONNS_PER_FORWARD: u32 = 64;
/// Default connections at once per sandbox, all forwards (`crabd --max-forward-conns`).
pub const DEFAULT_MAX_FORWARD_CONNS: u32 = 256;
/// Announced connections not yet accepted, per sandbox.
pub const MAX_PENDING_FORWARD_CONNS: u32 = 32;
/// An announced connection not accepted within this long is closed (`unclaimed`).
pub const FORWARD_UNCLAIMED_TIMEOUT_MS: u64 = 10_000;

/// `forward.listen` params (none).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForwardListenParams {}

impl ForwardListenParams {
    #[must_use]
    pub fn to_request(&self, id: impl Into<RequestId>) -> RpcRequest {
        RpcRequest::new(id, METHOD_FORWARD_LISTEN, Some(to_json(self)))
    }

    /// Decode from a `forward.listen` request. Any other method is
    /// [`ProtocolError::MethodNotFound`].
    pub fn from_request(req: &RpcRequest) -> Result<Self, ProtocolError> {
        if req.method != METHOD_FORWARD_LISTEN {
            return Err(ProtocolError::MethodNotFound(req.method.clone()));
        }
        from_params(req.params.clone().unwrap_or(Value::Null))
    }
}

/// `forward.listen` result. The connection stays in frame mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForwardListenAck {
    /// The forward ports `crabd` listens on.
    pub ports: Vec<u16>,
}

impl ForwardListenAck {
    #[must_use]
    pub fn to_value(&self) -> Value {
        to_json(self)
    }

    /// Decode a `forward.listen` `result` value (unknown fields are accepted).
    pub fn from_value(value: Value) -> Result<Self, ProtocolError> {
        serde_json::from_value(value)
            .map_err(|e| ProtocolError::Internal(format!("malformed `forward.listen` result: {e}")))
    }
}

/// `forward.incoming` notification params.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ForwardIncoming {
    /// Used once, by one `forward.accept` or `forward.reject`.
    pub conn_id: u64,
    /// The forward port the program connected to.
    pub port: u16,
}

/// `forward.reject` notification params.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForwardRejectParams {
    pub conn_id: u64,
}

fn notification(method: &str, params: Value) -> RpcNotification {
    RpcNotification {
        jsonrpc: JsonRpcVersion,
        method: method.to_owned(),
        params: Some(params),
    }
}

fn from_notification<T: serde::de::DeserializeOwned>(
    method: &str,
    n: &RpcNotification,
) -> Result<T, ProtocolError> {
    if n.method != method {
        return Err(ProtocolError::MethodNotFound(n.method.clone()));
    }
    from_params(n.params.clone().unwrap_or(Value::Null))
}

impl ForwardIncoming {
    #[must_use]
    pub fn to_notification(&self) -> RpcNotification {
        notification(METHOD_FORWARD_INCOMING, to_json(self))
    }

    /// Decode a `forward.incoming` notification (unknown fields are accepted).
    pub fn from_notification(n: &RpcNotification) -> Result<Self, ProtocolError> {
        from_notification(METHOD_FORWARD_INCOMING, n)
    }
}

impl ForwardRejectParams {
    #[must_use]
    pub fn to_notification(&self) -> RpcNotification {
        notification(METHOD_FORWARD_REJECT, to_json(self))
    }

    /// Decode a `forward.reject` notification.
    pub fn from_notification(n: &RpcNotification) -> Result<Self, ProtocolError> {
        from_notification(METHOD_FORWARD_REJECT, n)
    }
}

/// `forward.accept` params. An unknown or already used `conn_id` is `-32004`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForwardAcceptParams {
    pub conn_id: u64,
}

impl ForwardAcceptParams {
    #[must_use]
    pub fn to_request(&self, id: impl Into<RequestId>) -> RpcRequest {
        RpcRequest::new(id, METHOD_FORWARD_ACCEPT, Some(to_json(self)))
    }

    /// Decode from a `forward.accept` request. Any other method is
    /// [`ProtocolError::MethodNotFound`].
    pub fn from_request(req: &RpcRequest) -> Result<Self, ProtocolError> {
        if req.method != METHOD_FORWARD_ACCEPT {
            return Err(ProtocolError::MethodNotFound(req.method.clone()));
        }
        from_params(req.params.clone().unwrap_or(Value::Null))
    }
}

/// `forward.accept` result. `DATA` / `EOF` records follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ForwardAcceptAck {
    /// The forward port the program connected to.
    pub port: u16,
}

impl ForwardAcceptAck {
    #[must_use]
    pub fn to_value(&self) -> Value {
        to_json(self)
    }

    /// Decode a `forward.accept` `result` value (unknown fields are accepted).
    pub fn from_value(value: Value) -> Result<Self, ProtocolError> {
        serde_json::from_value(value)
            .map_err(|e| ProtocolError::Internal(format!("malformed `forward.accept` result: {e}")))
    }
}

/// `forward.stats` params (none).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForwardStatsParams {}

/// Why connections to a forward port were closed, counted since start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ForwardRefused {
    /// No `forward.listen` connection.
    pub no_listener: u64,
    /// Over a connection cap.
    pub cap: u64,
    /// Announced but not accepted within 10 s.
    pub unclaimed: u64,
    /// `forward.reject` from the host.
    pub rejected: u64,
}

/// Counters of one forward port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ForwardPortStats {
    pub port: u16,
    /// Connections handed to the host by `forward.accept`.
    pub accepted: u64,
    pub refused: ForwardRefused,
}

/// `forward.stats` result: one entry per forward port, as a JSON array.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ForwardStatsResult(pub Vec<ForwardPortStats>);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Call, RpcMessage, encode_frame};
    use serde_json::json;

    #[test]
    fn listen_round_trip() {
        let req = ForwardListenParams {}.to_request(1);
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({"jsonrpc":"2.0","id":1,"method":"forward.listen","params":{}})
        );
        assert_eq!(
            ForwardListenParams::from_request(&req).unwrap(),
            ForwardListenParams {}
        );
        let bare = RpcRequest::new(1, METHOD_FORWARD_LISTEN, None);
        ForwardListenParams::from_request(&bare).unwrap();
        let bad = RpcRequest::new(1, METHOD_FORWARD_LISTEN, Some(json!({"port": 1})));
        assert!(matches!(
            ForwardListenParams::from_request(&bad),
            Err(ProtocolError::InvalidParams(_))
        ));

        let ack = ForwardListenAck {
            ports: vec![5432, 8080],
        };
        assert_eq!(ForwardListenAck::from_value(ack.to_value()).unwrap(), ack);
        assert_eq!(
            ForwardListenAck::from_value(json!({"ports": [1], "x": 0})).unwrap(),
            ForwardListenAck { ports: vec![1] }
        );
    }

    #[test]
    fn incoming_and_reject_are_notifications() {
        let inc = ForwardIncoming {
            conn_id: u64::MAX,
            port: 5432,
        };
        let frame = encode_frame(&inc.to_notification()).unwrap();
        let RpcMessage::Notification(n) = RpcMessage::from_slice(&frame[4..]).unwrap() else {
            panic!("notification")
        };
        assert_eq!(n.method, METHOD_FORWARD_INCOMING);
        assert_eq!(ForwardIncoming::from_notification(&n).unwrap(), inc);
        assert!(matches!(
            ForwardRejectParams::from_notification(&n),
            Err(ProtocolError::MethodNotFound(_))
        ));

        let rej = ForwardRejectParams { conn_id: 7 };
        let frame = encode_frame(&rej.to_notification()).unwrap();
        let RpcMessage::Notification(n) = RpcMessage::from_slice(&frame[4..]).unwrap() else {
            panic!("notification")
        };
        assert_eq!(ForwardRejectParams::from_notification(&n).unwrap(), rej);
        let mut bad = n.clone();
        bad.params = Some(json!({"conn_id": 7, "reason": "x"}));
        assert!(ForwardRejectParams::from_notification(&bad).is_err());
    }

    #[test]
    fn accept_round_trip() {
        let p = ForwardAcceptParams { conn_id: 42 };
        let req = p.to_request("a");
        assert_eq!(ForwardAcceptParams::from_request(&req).unwrap(), p);
        for params in [
            json!({}),
            json!({"conn_id": -1}),
            json!({"conn_id": 1, "port": 2}),
        ] {
            let req = RpcRequest::new(1, METHOD_FORWARD_ACCEPT, Some(params.clone()));
            assert!(
                matches!(
                    ForwardAcceptParams::from_request(&req),
                    Err(ProtocolError::InvalidParams(_))
                ),
                "{params}"
            );
        }
        let other = RpcRequest::new(1, METHOD_FORWARD_LISTEN, Some(json!({"conn_id": 1})));
        assert!(matches!(
            ForwardAcceptParams::from_request(&other),
            Err(ProtocolError::MethodNotFound(_))
        ));
        let ack = ForwardAcceptAck { port: 9 };
        assert_eq!(ForwardAcceptAck::from_value(ack.to_value()).unwrap(), ack);
        assert!(ForwardAcceptAck::from_value(json!({})).is_err());
    }

    #[test]
    fn stats_wire_shape() {
        let s = ForwardStatsResult(vec![ForwardPortStats {
            port: 5432,
            accepted: 3,
            refused: ForwardRefused {
                no_listener: 1,
                cap: 2,
                unclaimed: 0,
                rejected: 4,
            },
        }]);
        let v = to_json(&s);
        assert_eq!(
            v,
            json!([{"port": 5432, "accepted": 3,
                    "refused": {"no_listener": 1, "cap": 2, "unclaimed": 0, "rejected": 4}}])
        );
        assert_eq!(serde_json::from_value::<ForwardStatsResult>(v).unwrap(), s);
    }

    #[test]
    fn connection_verbs_are_not_tools_or_calls() {
        for m in [
            METHOD_FORWARD_LISTEN,
            METHOD_FORWARD_ACCEPT,
            METHOD_FORWARD_INCOMING,
            METHOD_FORWARD_REJECT,
            METHOD_FORWARD_STATS,
        ] {
            assert!(!crate::TOOL_NAMES.contains(&m), "{m}");
        }
        for m in [METHOD_FORWARD_LISTEN, METHOD_FORWARD_ACCEPT] {
            assert!(matches!(
                Call::from_method_params(m, json!({"conn_id": 1})),
                Err(ProtocolError::MethodNotFound(_))
            ));
        }
    }
}
