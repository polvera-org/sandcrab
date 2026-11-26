//! Wire protocol, tool contracts and JSON Schema definitions shared between the Sandcrab
//! host control plane and the `crabd` guest daemon.
//!
//! # Wire format
//!
//! Each message is a frame: a `u32` big-endian payload length ([`HEADER_LEN`] bytes) followed
//! by a UTF-8 JSON-RPC 2.0 object, at most [`MAX_FRAME_LEN`] bytes. The same framing runs over
//! Unix sockets, TCP and vsock.
//!
//! ```text
//! -> {"jsonrpc":"2.0","id":1,"method":"bash","params":{"command":"ls"}}
//! <- {"jsonrpc":"2.0","id":1,"result":{"exit_code":0,"stdout":"...",...}}
//! <- {"jsonrpc":"2.0","id":2,"error":{"code":-32004,"message":"not found: ...","data":{...}}}
//! ```
//!
//! `method` is a tool name ([`TOOL_NAMES`]), [`METHOD_HELLO`] / [`METHOD_PING`], or one of
//! the non-tool methods [`METHOD_PROCESS_CANCEL`], [`METHOD_FS_IDENTITY`] and
//! [`METHOD_FORWARD_STATS`]. The connection-level verbs [`METHOD_AUTH`] ([`auth`]),
//! [`METHOD_TUNNEL_OPEN`] ([`tunnel`]), [`METHOD_PROCESS_OPEN`] ([`process`]) and
//! [`METHOD_FORWARD_LISTEN`] / [`METHOD_FORWARD_ACCEPT`] ([`forward`]) may only open a
//! connection. Process and forward connections then carry [`record`]s.
//! Typed layers: [`Call`] / [`Reply`] (all methods), [`ToolCall`] / [`ToolResult`] (tools).
//!
//! # Host flow
//!
//! ```
//! use sandcrab_protocol::*;
//!
//! let call = Call::Tool(ToolCall::Bash(BashParams {
//!     command: "echo hi".into(),
//!     cwd: None,
//!     env: Default::default(),
//!     timeout_ms: None,
//! }));
//! let frame = encode_frame(&call.to_request(1)).unwrap();
//!
//! // ... guest side ...
//! let mut dec = FrameDecoder::new();
//! dec.push(&frame);
//! let payload = dec.next_frame().unwrap().unwrap();
//! let RpcMessage::Request(req) = RpcMessage::from_slice(&payload).unwrap() else { panic!() };
//! assert_eq!(Call::from_request(&req).unwrap(), call);
//! ```

pub mod auth;
mod error;
pub mod forward;
mod framing;
pub mod init;
mod params;
pub mod process;
pub mod record;
mod results;
mod rpc;
mod schema;
mod tools;
pub mod tunnel;

pub use auth::{AUTH_TOKEN_BYTES, AUTH_TOKEN_HEX_LEN, AuthAck, AuthParams, AuthToken, METHOD_AUTH};
pub use error::{ProtocolError, RpcError, codes};
pub use forward::{
    ForwardAcceptAck, ForwardAcceptParams, ForwardIncoming, ForwardListenAck, ForwardListenParams,
    ForwardPortStats, ForwardRefused, ForwardRejectParams, ForwardStatsParams, ForwardStatsResult,
    METHOD_FORWARD_ACCEPT, METHOD_FORWARD_INCOMING, METHOD_FORWARD_LISTEN, METHOD_FORWARD_REJECT,
    METHOD_FORWARD_STATS,
};
pub use framing::{
    FrameDecoder, FrameError, HEADER_LEN, MAX_FRAME_LEN, decode_frame, encode_frame, parse_header,
    read_frame, read_message, write_frame,
};
pub use params::*;
pub use process::{
    ExecLimits, METHOD_PROCESS_CANCEL, METHOD_PROCESS_OPEN, OutputCap, Overflow,
    ProcessCancelParams, ProcessCancelResult, ProcessExit, ProcessOpenAck, ProcessOpenParams,
    SpawnTarget, StderrMode, StdinMode,
};
pub use record::{
    RECORD_HEADER_LEN, Record, RecordChannel, RecordDecoder, RecordError, RecordKind,
    encode_record, encode_record_header, parse_record_header,
};
pub use results::*;
pub use rpc::{
    Call, FileIdentity, FsIdentityParams, FsIdentityResult, JsonRpcVersion, METHOD_FS_IDENTITY,
    METHOD_HELLO, METHOD_PING, METHOD_STREAM_EVENT, OutputStream, Reply, RequestId,
    ResponsePayload, RpcMessage, RpcNotification, RpcRequest, RpcResponse, StreamEvent,
};
pub use schema::{MethodSchema, ToolDefinition, input_schema, method_schemas, output_schema};
pub use tools::{TOOL_NAMES, ToolCall, ToolResult, tool_definitions};
pub use tunnel::{METHOD_TUNNEL_OPEN, TunnelOpenAck, TunnelOpenParams};

/// Version of this wire protocol, reported by `hello`.
///
/// History: `0.1` initial; `0.2` adds the connection-level [`METHOD_AUTH`] frame and the
/// `UNAUTHORIZED` (`-32008`) error code; `0.3` adds `process.open` / `process.cancel`,
/// forwards, `fs.identity`, `hello` `features` and `forward_ports`, the error codes `-32009`
/// to `-32012`, and refuses a `hello` that names another version
/// ([`HelloParams::check_version`]).
pub const PROTOCOL_VERSION: &str = "0.3";

/// Values of `hello` [`HelloResult::features`].
pub mod features {
    /// `process.open` and `process.cancel` are served.
    pub const PROCESS_OPEN: &str = "process.open";
    /// `forward.listen`, `forward.accept` and `forward.stats` are served.
    pub const FORWARD: &str = "forward";
    /// Tree kill follows every descendant (`PR_SET_CHILD_SUBREAPER`).
    pub const TREE_KILL_SUBREAPER: &str = "tree_kill:subreaper";
    /// Fallback: tree kill reaches the process group only; a `setsid` escapes it.
    pub const TREE_KILL_PROCESS_GROUP: &str = "tree_kill:process_group";
}

/// Default limits referenced by tool descriptions. `crabd` enforces them.
pub mod limits {
    /// Default `bash` timeout.
    pub const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;
    /// Default `file_read` line limit.
    pub const DEFAULT_FILE_READ_LIMIT: u64 = 2_000;
    /// Max decoded size of an `upload` or `download` (base64 of it still fits in one frame).
    pub const MAX_TRANSFER_BYTES: u64 = 10 * 1024 * 1024;
}
