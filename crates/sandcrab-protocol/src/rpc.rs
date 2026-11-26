//! JSON-RPC 2.0 envelopes and the typed [`Call`] / [`Reply`] layer on top of them.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    PROTOCOL_VERSION, ProtocolError, RpcError, ToolCall, ToolResult,
    forward::{ForwardStatsParams, ForwardStatsResult, METHOD_FORWARD_STATS},
    params::{HelloParams, PingParams},
    process::{METHOD_PROCESS_CANCEL, ProcessCancelParams, ProcessCancelResult},
    results::{HelloResult, PingResult},
    tools::{from_params, to_json},
};

/// Method for the readiness / capability handshake.
pub const METHOD_HELLO: &str = "hello";
/// Method for a cheap liveness check.
pub const METHOD_PING: &str = "ping";
/// Notification method carrying a [`StreamEvent`].
pub const METHOD_STREAM_EVENT: &str = "stream_event";
/// Method that returns the `(dev, ino)` of paths in the sandbox. Not an LLM tool.
pub const METHOD_FS_IDENTITY: &str = "fs.identity";

/// `fs.identity` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsIdentityParams {
    /// Absolute paths in the sandbox. Not empty. Symlinks are followed, like `stat`.
    pub paths: Vec<String>,
}

impl FsIdentityParams {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.paths.is_empty() {
            return Err(ProtocolError::InvalidParams(
                "`paths` must not be empty".into(),
            ));
        }
        if let Some(p) = self
            .paths
            .iter()
            .find(|p| !p.starts_with('/') || p.contains('\0'))
        {
            return Err(ProtocolError::InvalidParams(format!(
                "`paths` must be absolute: {p:?}"
            )));
        }
        Ok(())
    }
}

/// The identity of one file: device and inode numbers, as `stat` reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// `fs.identity` result: one identity per path, in order, as a JSON array.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct FsIdentityResult(pub Vec<FileIdentity>);

impl HelloParams {
    /// The 0.3 version rule: a `hello` that names another protocol version is refused with
    /// [`ProtocolError::UnsupportedVersion`]; a `hello` without a version is answered.
    pub fn check_version(&self) -> Result<(), ProtocolError> {
        match self.protocol_version.as_deref() {
            Some(v) if v != PROTOCOL_VERSION => Err(ProtocolError::UnsupportedVersion {
                crabd: PROTOCOL_VERSION.to_owned(),
                host: v.to_owned(),
            }),
            _ => Ok(()),
        }
    }
}

/// The `"jsonrpc": "2.0"` marker. Deserialization rejects any other value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct JsonRpcVersion;

impl JsonRpcVersion {
    pub const STR: &'static str = "2.0";
}

impl Serialize for JsonRpcVersion {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(Self::STR)
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = String::deserialize(d)?;
        if v == Self::STR {
            Ok(Self)
        } else {
            Err(de::Error::invalid_value(
                de::Unexpected::Str(&v),
                &"\"2.0\"",
            ))
        }
    }
}

/// JSON-RPC request id: a string or an integer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl RequestId {
    /// Fresh random (UUID v4) string id.
    #[must_use]
    pub fn random() -> Self {
        Self::String(Uuid::new_v4().to_string())
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => f.write_str(s),
        }
    }
}

impl From<i64> for RequestId {
    fn from(n: i64) -> Self {
        Self::Number(n)
    }
}

impl From<String> for RequestId {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&str> for RequestId {
    fn from(s: &str) -> Self {
        Self::String(s.to_owned())
    }
}

/// JSON-RPC request: `{"jsonrpc":"2.0","id":..,"method":..,"params":{..}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl RpcRequest {
    #[must_use]
    pub fn new(id: impl Into<RequestId>, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC notification (no `id`, no response expected).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcNotification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Success or error body of a response, flattened as `"result"` or `"error"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsePayload {
    Result(Value),
    Error(RpcError),
}

/// JSON-RPC response. `id` is `null` only when the request id could not be read
/// (parse error / invalid request).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: Option<RequestId>,
    #[serde(flatten)]
    pub payload: ResponsePayload,
}

impl RpcResponse {
    #[must_use]
    pub fn success(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id: Some(id),
            payload: ResponsePayload::Result(result),
        }
    }

    #[must_use]
    pub fn failure(id: Option<RequestId>, error: &ProtocolError) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id,
            payload: ResponsePayload::Error(error.to_rpc_error()),
        }
    }

    /// Build a response from the outcome of handling a typed [`Call`].
    #[must_use]
    pub fn from_outcome(id: RequestId, outcome: Result<Reply, ProtocolError>) -> Self {
        match outcome {
            Ok(reply) => Self::success(id, reply.to_value()),
            Err(e) => Self::failure(Some(id), &e),
        }
    }

    /// Raw `result` value, or the typed error.
    pub fn into_result(self) -> Result<Value, ProtocolError> {
        match self.payload {
            ResponsePayload::Result(v) => Ok(v),
            ResponsePayload::Error(e) => Err(e.into()),
        }
    }
}

/// Any inbound JSON-RPC message, classified by shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RpcMessage {
    Request(RpcRequest),
    Notification(RpcNotification),
    Response(RpcResponse),
}

impl RpcMessage {
    /// Classify a frame payload. Invalid JSON maps to [`ProtocolError::Parse`];
    /// valid JSON that is not a JSON-RPC 2.0 message maps to [`ProtocolError::InvalidRequest`].
    pub fn from_slice(payload: &[u8]) -> Result<Self, ProtocolError> {
        let value: Value =
            serde_json::from_slice(payload).map_err(|e| ProtocolError::Parse(e.to_string()))?;
        Self::from_value(value)
    }

    /// Classify an already-parsed JSON value.
    pub fn from_value(value: Value) -> Result<Self, ProtocolError> {
        let Value::Object(obj) = &value else {
            return Err(ProtocolError::InvalidRequest(
                "message must be a JSON object".into(),
            ));
        };
        let invalid = |e: serde_json::Error| ProtocolError::InvalidRequest(e.to_string());
        if obj.contains_key("method") {
            if obj.contains_key("id") {
                serde_json::from_value(value)
                    .map(Self::Request)
                    .map_err(invalid)
            } else {
                serde_json::from_value(value)
                    .map(Self::Notification)
                    .map_err(invalid)
            }
        } else if obj.contains_key("result") || obj.contains_key("error") {
            serde_json::from_value(value)
                .map(Self::Response)
                .map_err(invalid)
        } else {
            Err(ProtocolError::InvalidRequest(
                "message has neither `method` nor `result`/`error`".into(),
            ))
        }
    }

    /// Best-effort id extraction from a raw payload, for error responses to bad requests.
    #[must_use]
    pub fn sniff_id(payload: &[u8]) -> Option<RequestId> {
        let value: Value = serde_json::from_slice(payload).ok()?;
        serde_json::from_value(value.get("id")?.clone()).ok()
    }
}

impl<'de> Deserialize<'de> for RpcMessage {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        Self::from_value(value).map_err(de::Error::custom)
    }
}

/// Every method `crabd` accepts on a tool connection, typed. Connection-level verbs
/// (`auth`, `tunnel.open`, `process.open`, `forward.listen`, `forward.accept`) are not calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Hello(HelloParams),
    Ping,
    ProcessCancel(ProcessCancelParams),
    FsIdentity(FsIdentityParams),
    ForwardStats,
    Tool(ToolCall),
}

impl Call {
    #[must_use]
    pub fn method(&self) -> &'static str {
        match self {
            Self::Hello(_) => METHOD_HELLO,
            Self::Ping => METHOD_PING,
            Self::ProcessCancel(_) => METHOD_PROCESS_CANCEL,
            Self::FsIdentity(_) => METHOD_FS_IDENTITY,
            Self::ForwardStats => METHOD_FORWARD_STATS,
            Self::Tool(call) => call.name(),
        }
    }

    #[must_use]
    pub fn to_params(&self) -> Value {
        match self {
            Self::Hello(p) => to_json(p),
            Self::Ping => to_json(&PingParams {}),
            Self::ProcessCancel(p) => to_json(p),
            Self::FsIdentity(p) => to_json(p),
            Self::ForwardStats => to_json(&ForwardStatsParams {}),
            Self::Tool(call) => call.to_params(),
        }
    }

    /// Decode from a method name and params (`null` = absent).
    pub fn from_method_params(method: &str, params: Value) -> Result<Self, ProtocolError> {
        match method {
            METHOD_HELLO => from_params(params).map(Self::Hello),
            METHOD_PING => from_params::<PingParams>(params).map(|_| Self::Ping),
            METHOD_PROCESS_CANCEL => from_params(params).map(Self::ProcessCancel),
            METHOD_FS_IDENTITY => {
                let p: FsIdentityParams = from_params(params)?;
                p.validate()?;
                Ok(Self::FsIdentity(p))
            }
            METHOD_FORWARD_STATS => {
                from_params::<ForwardStatsParams>(params).map(|_| Self::ForwardStats)
            }
            _ => ToolCall::from_method_params(method, params).map(Self::Tool),
        }
    }

    /// Decode from a request envelope.
    pub fn from_request(req: &RpcRequest) -> Result<Self, ProtocolError> {
        Self::from_method_params(&req.method, req.params.clone().unwrap_or(Value::Null))
    }

    /// Wrap in a request envelope.
    #[must_use]
    pub fn to_request(&self, id: impl Into<RequestId>) -> RpcRequest {
        RpcRequest::new(id, self.method(), Some(self.to_params()))
    }
}

impl From<ToolCall> for Call {
    fn from(call: ToolCall) -> Self {
        Self::Tool(call)
    }
}

/// Typed successful reply to a [`Call`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Hello(HelloResult),
    Ping(PingResult),
    ProcessCancel(ProcessCancelResult),
    FsIdentity(FsIdentityResult),
    ForwardStats(ForwardStatsResult),
    Tool(ToolResult),
}

impl Reply {
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Hello(r) => to_json(r),
            Self::Ping(r) => to_json(r),
            Self::ProcessCancel(r) => to_json(r),
            Self::FsIdentity(r) => to_json(r),
            Self::ForwardStats(r) => to_json(r),
            Self::Tool(r) => r.to_value(),
        }
    }

    /// Decode a `result` value given the method of the originating request.
    pub fn from_method_value(method: &str, value: Value) -> Result<Self, ProtocolError> {
        let malformed = |e: serde_json::Error| {
            ProtocolError::Internal(format!("malformed `{method}` result: {e}"))
        };
        match method {
            METHOD_HELLO => serde_json::from_value(value)
                .map(Self::Hello)
                .map_err(malformed),
            METHOD_PING => serde_json::from_value(value)
                .map(Self::Ping)
                .map_err(malformed),
            METHOD_PROCESS_CANCEL => serde_json::from_value(value)
                .map(Self::ProcessCancel)
                .map_err(malformed),
            METHOD_FS_IDENTITY => serde_json::from_value(value)
                .map(Self::FsIdentity)
                .map_err(malformed),
            METHOD_FORWARD_STATS => serde_json::from_value(value)
                .map(Self::ForwardStats)
                .map_err(malformed),
            _ => ToolResult::from_method_value(method, value).map(Self::Tool),
        }
    }
}

impl From<ToolResult> for Reply {
    fn from(r: ToolResult) -> Self {
        Self::Tool(r)
    }
}

/// Which output stream a chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Server-pushed event, sent as a `stream_event` notification.
///
/// Reserved and never sent. Protocol 0.3 streams output with `process.open` records instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Incremental output of an in-flight request or a background process.
    Output {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<RequestId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process_id: Option<String>,
        stream: OutputStream,
        data: String,
    },
    /// A background process exited.
    ProcessExited {
        process_id: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
}

impl StreamEvent {
    #[must_use]
    pub fn to_notification(&self) -> RpcNotification {
        RpcNotification {
            jsonrpc: JsonRpcVersion,
            method: METHOD_STREAM_EVENT.to_owned(),
            params: Some(to_json(self)),
        }
    }

    pub fn from_notification(n: &RpcNotification) -> Result<Self, ProtocolError> {
        if n.method != METHOD_STREAM_EVENT {
            return Err(ProtocolError::MethodNotFound(n.method.clone()));
        }
        serde_json::from_value(n.params.clone().unwrap_or(Value::Null))
            .map_err(|e| ProtocolError::InvalidParams(e.to_string()))
    }
}
