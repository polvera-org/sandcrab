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
//! `method` is a tool name ([`TOOL_NAMES`]) or [`METHOD_HELLO`] / [`METHOD_PING`].
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

mod error;
mod framing;
mod params;
mod results;
mod rpc;
mod schema;
mod tools;

pub use error::{ProtocolError, RpcError, codes};
pub use framing::{
    FrameDecoder, FrameError, HEADER_LEN, MAX_FRAME_LEN, decode_frame, encode_frame, parse_header,
    read_frame, read_message, write_frame,
};
pub use params::*;
pub use results::*;
pub use rpc::{
    Call, JsonRpcVersion, METHOD_HELLO, METHOD_PING, METHOD_STREAM_EVENT, OutputStream, Reply,
    RequestId, ResponsePayload, RpcMessage, RpcNotification, RpcRequest, RpcResponse, StreamEvent,
};
pub use schema::{ToolDefinition, input_schema, output_schema};
pub use tools::{TOOL_NAMES, ToolCall, ToolResult, tool_definitions};

/// Version of this wire protocol, reported by `hello`.
pub const PROTOCOL_VERSION: &str = "0.1";

/// Default limits referenced by tool descriptions. `crabd` enforces them.
pub mod limits {
    /// Default `bash` timeout.
    pub const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;
    /// Default `file_read` line limit.
    pub const DEFAULT_FILE_READ_LIMIT: u64 = 2_000;
    /// Max decoded size of an `upload` or `download` (base64 of it still fits in one frame).
    pub const MAX_TRANSFER_BYTES: u64 = 10 * 1024 * 1024;
}
