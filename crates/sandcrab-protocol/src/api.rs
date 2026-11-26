//! The client ↔ server API of `sandcrab-server`: pure data, no runtime.
//!
//! A Unix stream socket carries the same frames as `crabd` (u32 big-endian length, then one
//! JSON-RPC 2.0 object). Every connection starts with one frame, and its method decides the
//! kind of connection:
//!
//! | First frame | Kind | After the answer |
//! | --- | --- | --- |
//! | [`METHOD_SERVER_HELLO`] | control | requests, responses, notifications |
//! | [`METHOD_PROCESS_SPAWN`] | one process | [records](crate::record): `STDIN` / `STDIN_EOF` in, `STDOUT` / `STDERR` / `EXIT` out |
//! | [`METHOD_FORWARD_ATTACH`] | one forwarded connection | raw bytes both ways |
//!
//! Params reject unknown fields; results and notifications accept them. The process
//! connection reuses the 0.3 types: [`ProcessOpenParams`] (flattened into
//! [`ProcessSpawnParams`]), [`ProcessCancelParams`] / [`ProcessCancelResult`] and the `EXIT`
//! payload [`crate::ProcessExit`]. The server's own errors are [`ApiError`]; `crabd`'s pass
//! through unchanged for `process.spawn` and `process.cancel`.
//!
//! [`server_api_schema`] exports one JSON Schema document for the whole API, from which the
//! TypeScript SDK generates its types.

use std::collections::BTreeMap;

use schemars::{
    JsonSchema,
    r#gen::{SchemaGenerator, SchemaSettings},
    schema::{InstanceType, NumberValidation, Schema, SchemaObject},
};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{
    ForwardRefused, JsonRpcVersion, ProcessCancelParams, ProcessCancelResult, ProcessExit,
    ProcessOpenParams, ProtocolError, RequestId, RpcError, RpcNotification, RpcRequest,
    tools::{from_params, to_json},
};

/// Version of the server API, `"major.minor"`, reported by `server.hello`.
pub const API_VERSION: &str = "1.0";

/// Values of [`ServerHelloResult::features`]: optional abilities of the server.
pub mod features {
    /// `forwards` on `sandbox.create`, `forward.serve`, `forward.attach`.
    pub const FORWARDS: &str = "forwards";
    /// `mounts` on `sandbox.create`.
    pub const MOUNTS: &str = "mounts";
    /// `server.selftest` and the self-test summary in `server.capabilities`.
    pub const SELFTEST: &str = "selftest";
    /// Every feature of [`super::API_VERSION`].
    pub const ALL: &[&str] = &[FORWARDS, MOUNTS, SELFTEST];
}

// Methods on a control connection.
pub const METHOD_SERVER_HELLO: &str = "server.hello";
pub const METHOD_SERVER_CAPABILITIES: &str = "server.capabilities";
pub const METHOD_SERVER_SELFTEST: &str = "server.selftest";
pub const METHOD_SANDBOX_CREATE: &str = "sandbox.create";
pub const METHOD_SANDBOX_GET: &str = "sandbox.get";
pub const METHOD_SANDBOX_LIST: &str = "sandbox.list";
pub const METHOD_SANDBOX_SET_TTL: &str = "sandbox.set_ttl";
pub const METHOD_SANDBOX_DESTROY: &str = "sandbox.destroy";
pub const METHOD_SANDBOX_DESTROY_BY_LABEL: &str = "sandbox.destroy_by_label";
pub const METHOD_PROCESS_CANCEL: &str = crate::METHOD_PROCESS_CANCEL;
pub const METHOD_FORWARD_SERVE: &str = "forward.serve";
pub const METHOD_FORWARD_REJECT: &str = "forward.reject";

// First frames of the other connection kinds.
pub const METHOD_PROCESS_SPAWN: &str = "process.spawn";
pub const METHOD_FORWARD_ATTACH: &str = "forward.attach";

// Notifications from the server, on a control connection.
pub const NOTIFY_FORWARD_INCOMING: &str = "forward.incoming";
pub const NOTIFY_SANDBOX_DESTROYED: &str = "sandbox.destroyed";
pub const NOTIFY_SERVER_SHUTDOWN: &str = "server.shutdown";

/// Error codes the server adds. `crabd`'s codes ([`crate::codes`]) pass through for
/// `process.spawn` and `process.cancel`.
pub mod codes {
    /// The backend could not start or reach the sandbox. Nothing ran; trying again may work.
    pub const UNAVAILABLE: i64 = -32101;
    /// No such sandbox.
    pub const SANDBOX_NOT_FOUND: i64 = -32102;
    /// A mount outside the allowed roots, nested, through a symlink, over the cap, or one that
    /// touches the server's own directories.
    pub const MOUNT_NOT_ALLOWED: i64 = -32103;
    /// A server limit (sandboxes, processes, resources, connections).
    pub const LIMIT_EXCEEDED: i64 = -32104;
    /// The backend cannot enforce what was asked.
    pub const POLICY_UNENFORCEABLE: i64 = -32105;
    /// Template not found, unpinned, or its pin does not match.
    pub const TEMPLATE_ERROR: i64 = -32106;
    /// The server no longer accepts this.
    pub const SHUTTING_DOWN: i64 = -32107;
    /// A `process_id` used twice, a sandbox that is being destroyed.
    pub const CONFLICT: i64 = -32108;
    /// `server.hello` with an `api_version` the server does not serve.
    pub const UNSUPPORTED_VERSION: i64 = crate::codes::UNSUPPORTED_VERSION;
    /// The peer's uid is not allowed. One frame, then close.
    pub const UNAUTHORIZED: i64 = crate::codes::UNAUTHORIZED;
}

// ---------------------------------------------------------------------------------------------
// server.hello, server.capabilities, server.selftest
// ---------------------------------------------------------------------------------------------

/// `server.hello` params: the first frame of a control connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerHelloParams {
    /// Free-form client identifier, e.g. `@sandcrab/sdk/1.0.0`.
    pub client: String,
    /// API version the client speaks, `"major.minor"`.
    pub api_version: String,
}

impl ServerHelloParams {
    /// The server's rule (design §5.11): a server at `M.N` serves clients of major `M` and
    /// minor `N - 1` or later. Another major, an older minor or a malformed version is
    /// [`ApiError::UnsupportedVersion`].
    pub fn check_version(&self) -> Result<(), ApiError> {
        let refuse = || {
            ApiError::UnsupportedVersion(UnsupportedVersionData {
                server: API_VERSION.to_owned(),
                client: self.api_version.clone(),
            })
        };
        let (smaj, smin) = parse_version(API_VERSION).expect("API_VERSION is major.minor");
        let (cmaj, cmin) = parse_version(&self.api_version).ok_or_else(refuse)?;
        if cmaj == smaj && cmin >= smin.saturating_sub(1) {
            Ok(())
        } else {
            Err(refuse())
        }
    }
}

fn parse_version(v: &str) -> Option<(u32, u32)> {
    let (major, minor) = v.split_once('.')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(major) || !digits(minor) {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// The server's limits (design §5.6). Over a limit is `LIMIT_EXCEEDED`; nothing queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServerLimits {
    /// Connections of all kinds.
    pub max_connections: u32,
    /// Requests in flight per control connection; beyond that the server stops reading.
    pub max_requests_in_flight: u32,
    pub max_sandboxes: u32,
    pub max_processes_per_sandbox: u32,
    pub max_mounts: u32,
    pub max_vcpu: u32,
    pub max_memory_mb: u64,
    pub max_pids: u64,
    pub max_nofile: u64,
    /// TTL of a sandbox created without `ttl_secs`.
    pub default_ttl_secs: u64,
    pub max_ttl_secs: u64,
}

/// `server.hello` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServerHelloResult {
    /// `sandcrab-server` build version.
    pub server_version: String,
    /// API version the server speaks ([`API_VERSION`]).
    pub api_version: String,
    /// Optional abilities ([`features`]).
    pub features: Vec<String>,
    /// The backend this server runs: `docker`, `libkrun` or `firecracker`.
    pub backend: String,
    pub limits: ServerLimits,
}

/// `server.capabilities` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesParams {
    /// The template whose self-test summary to include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// The last self-test of one template, in short.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SelftestSummary {
    pub template: String,
    /// True only when every required line passed.
    pub ok: bool,
    /// One sentence naming the first line that did not pass; `null` when `ok`.
    pub reason: Option<String>,
    /// Age of the report.
    pub age_secs: u64,
}

/// `server.capabilities` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Capabilities {
    pub backend: String,
    pub hardware_isolated: bool,
    pub multi_tenant_safe: bool,
    pub supports_host_dir_mounts: bool,
    pub supports_forwards: bool,
    /// `subreaper` or `process_group` (the named fallback).
    pub tree_kill: String,
    /// What `RLIMIT_NPROC` counts: `host_uid` (Docker: every process of the uid on the host)
    /// or `sandbox`.
    pub nproc_scope: String,
    /// Whether the mount identity is checked again after start (design §6.3).
    pub mount_identity_checked: bool,
    pub max_mounts: u32,
    /// For the template named in the request. `null`: no template named, or never run.
    pub selftest: Option<SelftestSummary>,
}

/// `server.selftest` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SelftestParams {
    pub template: String,
    /// Return the last report for this template when it is younger than this. Default: always
    /// run a fresh one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_secs: Option<u64>,
}

/// How one self-test line ended. A timeout is never a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SelftestStatus {
    Pass,
    Fail,
    /// The probe hit its deadline, hung or never started.
    Inconclusive,
}

/// One line of a self-test report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SelftestLine {
    /// The line, e.g. `net-external-refused`.
    pub id: String,
    pub status: SelftestStatus,
    /// Whether `ok` depends on this line.
    pub required: bool,
    /// One sentence for a reader.
    pub detail: String,
    /// What the probe tried and the `errno` it got, as the probe reported it.
    pub evidence: Value,
}

/// `server.selftest` result: a report for one template (design §8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SelftestReport {
    /// True only when every required line is `pass`.
    pub ok: bool,
    /// One sentence naming the first line that did not pass; `null` when `ok`.
    pub reason: Option<String>,
    pub backend: String,
    pub template: String,
    /// RFC 3339.
    pub started_at: String,
    pub duration_ms: u64,
    pub lines: Vec<SelftestLine>,
}

// ---------------------------------------------------------------------------------------------
// sandbox.*
// ---------------------------------------------------------------------------------------------

/// Read-only or read-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum MountMode {
    #[serde(rename = "ro")]
    ReadOnly,
    #[serde(rename = "rw")]
    ReadWrite,
}

/// A host folder in the sandbox, at the same absolute path (design §6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MountSpec {
    /// Absolute, resolved, no symlink on the way. The client's path, the host path and the
    /// guest path.
    pub path: String,
    pub mode: MountMode,
}

/// The sandbox's network.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// Loopback only; forwards are the way out.
    #[default]
    Disabled,
    Unrestricted,
}

/// A forward: a loopback port in the sandbox whose connections the serving client routes.
/// The target is not part of the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForwardSpec {
    pub guest_port: u16,
    /// Connections at once. Default 64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
}

/// Sandbox resources. Each one left out is the server's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_mb: Option<u64>,
    /// The sandbox's pids limit (a cgroup limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids_max: Option<u64>,
    /// `RLIMIT_NOFILE` of the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nofile: Option<u64>,
}

/// `sandbox.create` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxCreate {
    pub template: String,
    /// At most 16. Key `[a-z0-9][a-z0-9._-]{0,62}`, not starting with `sandcrab.`. Value at
    /// most 256 bytes.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Absent: the server's default. Explicit `null`: no TTL.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub ttl_secs: Option<Option<u64>>,
    /// At most 16, none nested in another.
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    #[serde(default)]
    pub network: NetworkMode,
    /// At most 16.
    #[serde(default)]
    pub forwards: Vec<ForwardSpec>,
    /// `"uid:gid"`. Default: the connecting peer's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default)]
    pub resources: Resources,
    /// Drop `noexec` from `/tmp`.
    #[serde(default)]
    pub tmp_exec: bool,
    /// Size of `/tmp` (counts against the sandbox's memory). Default 256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmp_size_mb: Option<u64>,
}

impl SandboxCreate {
    /// A create of `template` with every other field at its default.
    #[must_use]
    pub fn new(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
            labels: BTreeMap::new(),
            ttl_secs: None,
            mounts: Vec::new(),
            network: NetworkMode::default(),
            forwards: Vec::new(),
            user: None,
            resources: Resources::default(),
            tmp_exec: false,
            tmp_size_mb: None,
        }
    }
}

/// A field that is present (`Some`), with a value or `null`; absence is the `default`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

/// Whether a sandbox runs or is on its way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    Running,
    Destroying,
}

/// A forward of a sandbox with its counters since start (design §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ForwardInfo {
    pub guest_port: u16,
    /// Connections handed to the serving client.
    pub accepted: u64,
    /// Connections closed, by reason. A refusal looks like a flaky network from inside.
    pub refused: ForwardRefused,
}

/// A sandbox, as `sandbox.create`, `sandbox.get`, `sandbox.list` and `sandbox.set_ttl`
/// answer it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxInfo {
    pub sandbox_id: String,
    pub template: String,
    pub labels: BTreeMap<String, String>,
    pub state: SandboxState,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339. `null`: no TTL.
    pub expires_at: Option<String>,
    pub backend: String,
    pub mounts: Vec<MountSpec>,
    pub forwards: Vec<ForwardInfo>,
    /// Whether a control connection serves the forwards now.
    pub forwards_served: bool,
}

/// `sandbox.get` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxGetParams {
    pub sandbox_id: String,
}

/// `sandbox.list` params. Every filter given must match.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxListParams {
    /// Every label given must match exactly.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// RFC 3339. Only sandboxes created after this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_after: Option<String>,
    /// RFC 3339. Only sandboxes created before this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_before: Option<String>,
}

/// `sandbox.list` result, as a JSON array.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SandboxList(pub Vec<SandboxInfo>);

/// `sandbox.set_ttl` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxSetTtlParams {
    pub sandbox_id: String,
    /// Required: seconds from now, or `null` for no TTL.
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "required_nullable_u64")]
    pub ttl_secs: Option<u64>,
}

/// A `u64` or `null`. With `schema_with` the field counts as required, which it is.
fn required_nullable_u64(_: &mut SchemaGenerator) -> Schema {
    SchemaObject {
        instance_type: Some(vec![InstanceType::Integer, InstanceType::Null].into()),
        number: Some(Box::new(NumberValidation {
            minimum: Some(0.0),
            ..NumberValidation::default()
        })),
        ..SchemaObject::default()
    }
    .into()
}

/// `sandbox.destroy` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxDestroyParams {
    pub sandbox_id: String,
}

/// `sandbox.destroy` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxDestroyResult {
    /// False: it was already gone.
    pub destroyed: bool,
}

/// `sandbox.destroy_by_label` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DestroyByLabelParams {
    /// Not empty. Every label must match exactly.
    pub labels: BTreeMap<String, String>,
}

/// `sandbox.destroy_by_label` result (design §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DestroyByLabelResult {
    /// Sandboxes that matched at the first look.
    pub matched: u64,
    /// Of those, destroyed.
    pub destroyed: u64,
    /// Sandbox ids that still match: a failed destroy, or one created meanwhile.
    pub remaining: Vec<String>,
    /// True only when every destroy succeeded and a second look finds no match.
    pub empty: bool,
}

// ---------------------------------------------------------------------------------------------
// forward.serve, forward.reject, forward.attach, forward.incoming
// ---------------------------------------------------------------------------------------------

/// `forward.serve` params: this control connection now serves the sandbox's forwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForwardServeParams {
    pub sandbox_id: String,
}

/// `forward.serve` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForwardServeResult {
    /// The sandbox's forward ports.
    pub ports: Vec<u16>,
}

/// `forward.reject` params: the client could not reach its target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForwardRejectParams {
    /// From `forward.incoming`.
    pub conn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// An empty object: the result of `forward.reject` and `forward.attach`, and the error data
/// of `SHUTTING_DOWN` and `UNAUTHORIZED`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct Empty {}

/// `forward.attach` params: the first frame of a forwarded connection. Raw bytes follow the
/// answer. Not attached within 10 s: the connection is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForwardAttachParams {
    /// From `forward.incoming`. Used once.
    pub conn_id: String,
}

/// `forward.incoming` notification: a program connected to a forward. The client connects
/// its target, then attaches, or rejects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForwardIncoming {
    /// 128 random bits, hex.
    pub conn_id: String,
    pub sandbox_id: String,
    pub guest_port: u16,
}

// ---------------------------------------------------------------------------------------------
// process.spawn
// ---------------------------------------------------------------------------------------------

/// `process.spawn` params: the first frame of a process connection. `sandbox_id`, then the
/// fields of [`ProcessOpenParams`] at the same level. Unknown fields are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "Map<String, Value>")]
#[schemars(deny_unknown_fields)]
pub struct ProcessSpawnParams {
    pub sandbox_id: String,
    /// Sent to the guest as `process.open` with the same id and values.
    #[serde(flatten)]
    pub open: ProcessOpenParams,
}

impl TryFrom<Map<String, Value>> for ProcessSpawnParams {
    type Error = String;

    fn try_from(mut map: Map<String, Value>) -> Result<Self, String> {
        let sandbox_id = match map.remove("sandbox_id") {
            Some(Value::String(s)) => s,
            Some(_) => return Err("`sandbox_id` must be a string".into()),
            None => return Err("missing field `sandbox_id`".into()),
        };
        // `ProcessOpenParams` rejects every field it does not know.
        let open = serde_json::from_value(Value::Object(map)).map_err(|e| e.to_string())?;
        Ok(Self { sandbox_id, open })
    }
}

impl ProcessSpawnParams {
    /// Checks that need no knowledge of the sandbox ([`ProcessOpenParams::validate`]).
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.sandbox_id.is_empty() {
            return Err(ProtocolError::InvalidParams(
                "`sandbox_id` must not be empty".into(),
            ));
        }
        self.open.validate()
    }

    #[must_use]
    pub fn to_request(&self, id: impl Into<RequestId>) -> RpcRequest {
        RpcRequest::new(id, METHOD_PROCESS_SPAWN, Some(to_json(self)))
    }

    /// Decode and validate from a `process.spawn` request. Any other method is
    /// [`ProtocolError::MethodNotFound`].
    pub fn from_request(req: &RpcRequest) -> Result<Self, ProtocolError> {
        if req.method != METHOD_PROCESS_SPAWN {
            return Err(ProtocolError::MethodNotFound(req.method.clone()));
        }
        let p: Self = decode_params(req.params.clone())?;
        p.validate()?;
        Ok(p)
    }
}

/// `process.spawn` result, sent after the guest's ack. Records follow. No pid leaves the
/// server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessSpawnAck {
    pub process_id: String,
}

// ---------------------------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------------------------

/// Why a sandbox ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DestroyReason {
    /// `sandbox.destroy`, `sandbox.destroy_by_label` or a server stop.
    Destroyed,
    /// Its TTL passed.
    Ttl,
    /// The backend lost it (container or VM gone).
    BackendExit,
}

/// `sandbox.destroyed` notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxDestroyed {
    pub sandbox_id: String,
    pub reason: DestroyReason,
}

/// `server.shutdown` notification: the server stops; every sandbox will be destroyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServerShutdown {
    pub grace_ms: u64,
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// `data` of `UNAVAILABLE`, `POLICY_UNENFORCEABLE` and `CONFLICT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DetailData {
    pub detail: String,
}

/// `data` of `SANDBOX_NOT_FOUND`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxNotFoundData {
    pub sandbox_id: String,
}

/// `data` of `MOUNT_NOT_ALLOWED`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MountNotAllowedData {
    pub path: String,
    pub reason: String,
    /// The mount cap, when that is the reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<u32>,
}

/// `data` of `LIMIT_EXCEEDED`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LimitExceededData {
    /// `sandboxes`, `processes`, `connections`, `vcpu`, ...
    pub what: String,
    pub limit: u64,
}

/// What is wrong with a template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TemplateErrorKind {
    NotFound,
    Unpinned,
    PinMismatch,
}

/// `data` of `TEMPLATE_ERROR`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TemplateErrorData {
    pub kind: TemplateErrorKind,
    pub template: String,
}

/// `data` of `UNSUPPORTED_VERSION`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UnsupportedVersionData {
    /// The server's API version.
    pub server: String,
    /// The client's, as it sent it.
    pub client: String,
}

/// An error of the server API. [`ApiError::Protocol`] carries every other code, including
/// `crabd`'s for `process.spawn` and `process.cancel`, unchanged.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ApiError {
    #[error("sandbox unavailable: {}", .0.detail)]
    Unavailable(DetailData),
    #[error("no such sandbox: {}", .0.sandbox_id)]
    SandboxNotFound(SandboxNotFoundData),
    #[error("mount not allowed: {}: {}", .0.path, .0.reason)]
    MountNotAllowed(MountNotAllowedData),
    #[error("limit exceeded: at most {} {}", .0.limit, .0.what)]
    LimitExceeded(LimitExceededData),
    #[error("policy unenforceable: {}", .0.detail)]
    PolicyUnenforceable(DetailData),
    #[error("template `{}`: {:?}", .0.template, .0.kind)]
    TemplateError(TemplateErrorData),
    #[error("the server is shutting down")]
    ShuttingDown,
    #[error("conflict: {}", .0.detail)]
    Conflict(DetailData),
    #[error(
        "unsupported API version: the server speaks {}, the client {}",
        .0.server,
        .0.client
    )]
    UnsupportedVersion(UnsupportedVersionData),
    #[error("unauthorized")]
    Unauthorized,
    #[error(transparent)]
    Protocol(ProtocolError),
}

impl ApiError {
    /// JSON-RPC error code.
    #[must_use]
    pub fn code(&self) -> i64 {
        match self {
            Self::Unavailable(_) => codes::UNAVAILABLE,
            Self::SandboxNotFound(_) => codes::SANDBOX_NOT_FOUND,
            Self::MountNotAllowed(_) => codes::MOUNT_NOT_ALLOWED,
            Self::LimitExceeded(_) => codes::LIMIT_EXCEEDED,
            Self::PolicyUnenforceable(_) => codes::POLICY_UNENFORCEABLE,
            Self::TemplateError(_) => codes::TEMPLATE_ERROR,
            Self::ShuttingDown => codes::SHUTTING_DOWN,
            Self::Conflict(_) => codes::CONFLICT,
            Self::UnsupportedVersion(_) => codes::UNSUPPORTED_VERSION,
            Self::Unauthorized => codes::UNAUTHORIZED,
            Self::Protocol(e) => e.code(),
        }
    }

    /// Convert into a JSON-RPC error object; `data` is the variant's data type.
    #[must_use]
    pub fn to_rpc_error(&self) -> RpcError {
        let data = match self {
            Self::Unavailable(d) | Self::PolicyUnenforceable(d) | Self::Conflict(d) => to_json(d),
            Self::SandboxNotFound(d) => to_json(d),
            Self::MountNotAllowed(d) => to_json(d),
            Self::LimitExceeded(d) => to_json(d),
            Self::TemplateError(d) => to_json(d),
            Self::UnsupportedVersion(d) => to_json(d),
            Self::ShuttingDown | Self::Unauthorized => json!({}),
            Self::Protocol(e) => return e.to_rpc_error(),
        };
        RpcError {
            code: self.code(),
            message: self.to_string(),
            data: Some(data),
        }
    }

    /// Rebuild from a JSON-RPC error object. A server code whose `data` does not parse, and
    /// every other code, becomes [`ApiError::Protocol`].
    #[must_use]
    pub fn from_rpc_error(err: RpcError) -> Self {
        fn data<T: DeserializeOwned>(err: &RpcError) -> Option<T> {
            serde_json::from_value(err.data.clone()?).ok()
        }
        let typed = match err.code {
            codes::UNAVAILABLE => data(&err).map(Self::Unavailable),
            codes::SANDBOX_NOT_FOUND => data(&err).map(Self::SandboxNotFound),
            codes::MOUNT_NOT_ALLOWED => data(&err).map(Self::MountNotAllowed),
            codes::LIMIT_EXCEEDED => data(&err).map(Self::LimitExceeded),
            codes::POLICY_UNENFORCEABLE => data(&err).map(Self::PolicyUnenforceable),
            codes::TEMPLATE_ERROR => data(&err).map(Self::TemplateError),
            codes::SHUTTING_DOWN => Some(Self::ShuttingDown),
            codes::CONFLICT => data(&err).map(Self::Conflict),
            codes::UNSUPPORTED_VERSION => data(&err).map(Self::UnsupportedVersion),
            codes::UNAUTHORIZED if err.data.as_ref().is_none_or(|d| d == &json!({})) => {
                Some(Self::Unauthorized)
            }
            _ => None,
        };
        typed.unwrap_or_else(|| Self::Protocol(ProtocolError::from_rpc_error(err)))
    }
}

impl From<ProtocolError> for ApiError {
    fn from(e: ProtocolError) -> Self {
        Self::Protocol(e)
    }
}

impl From<ApiError> for RpcError {
    fn from(e: ApiError) -> Self {
        e.to_rpc_error()
    }
}

// ---------------------------------------------------------------------------------------------
// Envelopes
// ---------------------------------------------------------------------------------------------

/// Decode request params; anything that does not fit is [`ProtocolError::InvalidParams`].
/// Absent params are an empty object.
pub fn decode_params<T: DeserializeOwned>(params: Option<Value>) -> Result<T, ProtocolError> {
    from_params(params.unwrap_or(Value::Null))
}

/// A request of `method` with `params`.
#[must_use]
pub fn request<T: Serialize>(id: impl Into<RequestId>, method: &str, params: &T) -> RpcRequest {
    RpcRequest::new(id, method, Some(to_json(params)))
}

/// A notification of `method` with `params`.
#[must_use]
pub fn notification<T: Serialize>(method: &str, params: &T) -> RpcNotification {
    RpcNotification {
        jsonrpc: JsonRpcVersion,
        method: method.to_owned(),
        params: Some(to_json(params)),
    }
}

// ---------------------------------------------------------------------------------------------
// Schema document
// ---------------------------------------------------------------------------------------------

/// The JSON Schema document of the whole server API: `$defs` for every params, result, error
/// data and notification type (and the `EXIT` record payload), plus tables that name them:
/// `methods` (`name → {kind: "control" | "connection", params, result}`), `notifications`
/// (`name → {params}`), `errors` (`NAME → {code, data}`) and `records`.
///
/// Optional fields of params may be absent or `null`; nullable fields of results are always
/// present.
#[must_use]
pub fn server_api_schema() -> Value {
    let settings = SchemaSettings::draft2019_09().with(|s| {
        s.definitions_path = "#/$defs/".to_owned();
        s.option_add_null_type = true;
        s.option_nullable = false;
    });
    let mut g = settings.into_generator();

    fn r<T: JsonSchema>(g: &mut SchemaGenerator) -> Value {
        let mut v = serde_json::to_value(g.subschema_for::<T>()).expect("schema serializes");
        crate::schema::clean(&mut v);
        v
    }
    fn m<P: JsonSchema, R: JsonSchema>(g: &mut SchemaGenerator, kind: &str) -> Value {
        json!({"kind": kind, "params": r::<P>(g), "result": r::<R>(g)})
    }

    let methods = json!({
        METHOD_SERVER_HELLO: m::<ServerHelloParams, ServerHelloResult>(&mut g, "control"),
        METHOD_SERVER_CAPABILITIES: m::<CapabilitiesParams, Capabilities>(&mut g, "control"),
        METHOD_SERVER_SELFTEST: m::<SelftestParams, SelftestReport>(&mut g, "control"),
        METHOD_SANDBOX_CREATE: m::<SandboxCreate, SandboxInfo>(&mut g, "control"),
        METHOD_SANDBOX_GET: m::<SandboxGetParams, SandboxInfo>(&mut g, "control"),
        METHOD_SANDBOX_LIST: m::<SandboxListParams, SandboxList>(&mut g, "control"),
        METHOD_SANDBOX_SET_TTL: m::<SandboxSetTtlParams, SandboxInfo>(&mut g, "control"),
        METHOD_SANDBOX_DESTROY: m::<SandboxDestroyParams, SandboxDestroyResult>(&mut g, "control"),
        METHOD_SANDBOX_DESTROY_BY_LABEL:
            m::<DestroyByLabelParams, DestroyByLabelResult>(&mut g, "control"),
        METHOD_PROCESS_CANCEL: m::<ProcessCancelParams, ProcessCancelResult>(&mut g, "control"),
        METHOD_FORWARD_SERVE: m::<ForwardServeParams, ForwardServeResult>(&mut g, "control"),
        METHOD_FORWARD_REJECT: m::<ForwardRejectParams, Empty>(&mut g, "control"),
        METHOD_PROCESS_SPAWN: m::<ProcessSpawnParams, ProcessSpawnAck>(&mut g, "connection"),
        METHOD_FORWARD_ATTACH: m::<ForwardAttachParams, Empty>(&mut g, "connection"),
    });
    let notifications = json!({
        NOTIFY_FORWARD_INCOMING: {"params": r::<ForwardIncoming>(&mut g)},
        NOTIFY_SANDBOX_DESTROYED: {"params": r::<SandboxDestroyed>(&mut g)},
        NOTIFY_SERVER_SHUTDOWN: {"params": r::<ServerShutdown>(&mut g)},
    });
    let detail = r::<DetailData>(&mut g);
    let empty = r::<Empty>(&mut g);
    let errors = json!({
        "UNAVAILABLE": {"code": codes::UNAVAILABLE, "data": detail},
        "SANDBOX_NOT_FOUND": {"code": codes::SANDBOX_NOT_FOUND, "data": r::<SandboxNotFoundData>(&mut g)},
        "MOUNT_NOT_ALLOWED": {"code": codes::MOUNT_NOT_ALLOWED, "data": r::<MountNotAllowedData>(&mut g)},
        "LIMIT_EXCEEDED": {"code": codes::LIMIT_EXCEEDED, "data": r::<LimitExceededData>(&mut g)},
        "POLICY_UNENFORCEABLE": {"code": codes::POLICY_UNENFORCEABLE, "data": detail},
        "TEMPLATE_ERROR": {"code": codes::TEMPLATE_ERROR, "data": r::<TemplateErrorData>(&mut g)},
        "SHUTTING_DOWN": {"code": codes::SHUTTING_DOWN, "data": empty},
        "CONFLICT": {"code": codes::CONFLICT, "data": detail},
        "UNSUPPORTED_VERSION": {"code": codes::UNSUPPORTED_VERSION, "data": r::<UnsupportedVersionData>(&mut g)},
        "UNAUTHORIZED": {"code": codes::UNAUTHORIZED, "data": empty},
    });
    let records = json!({
        "EXIT": {"kind": crate::RecordKind::Exit.byte(), "payload": r::<ProcessExit>(&mut g)},
    });

    let mut defs = Map::new();
    for (name, schema) in g.take_definitions() {
        let mut v = serde_json::to_value(schema).expect("schema serializes");
        crate::schema::clean(&mut v);
        defs.insert(name, v);
    }
    json!({
        "$schema": "https://json-schema.org/draft/2019-09/schema",
        "title": "Sandcrab server API",
        "api_version": API_VERSION,
        "features": features::ALL,
        "methods": methods,
        "notifications": notifications,
        "errors": errors,
        "records": records,
        "$defs": defs,
    })
}
