//! A control connection (design §5.3): `server.hello` first, then requests, answered
//! concurrently up to [`MAX_REQUESTS_IN_FLIGHT`] (beyond that the server stops reading).
//!
//! Every sandbox belongs to the peer uid that created it. Another uid gets
//! `SANDBOX_NOT_FOUND` for it and never sees it in a list. Params are never logged.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sandcrab_sdk::protocol::api::{
    self, ApiError, Capabilities, CapabilitiesParams, DestroyByLabelParams, DestroyByLabelResult,
    DetailData, ForwardInfo, LimitExceededData, MountMode, MountNotAllowedData, MountSpec,
    NetworkMode, SandboxCreate, SandboxDestroyParams, SandboxDestroyResult, SandboxGetParams,
    SandboxInfo, SandboxListParams, SandboxNotFoundData, SandboxSetTtlParams, SandboxState,
    ServerHelloParams, ServerHelloResult, TemplateErrorData, TemplateErrorKind, decode_params,
};
use sandcrab_sdk::protocol::{ForwardRefused, ProtocolError, RpcMessage, RpcRequest, RpcResponse};
use sandcrab_sdk::{
    ForwardSpec, HostMount, NetworkPolicy, NprocScope, ResourceOverrides, SandboxConfig,
    SandboxError, SandboxFilter, SandboxId, UserSpec,
};
use serde::Serialize;
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::sync::{Semaphore, mpsc};

use crate::listener::failure;
use crate::wire::{frame, read_frame, write_frame};
use crate::{MAX_MEMORY_MB, MAX_NOFILE, MAX_PIDS, MAX_REQUESTS_IN_FLIGHT, MAX_VCPU, Peer, Shared};

/// Features this server serves now ([`api::features`]). `forwards` and `selftest` come with
/// the steps that build them.
pub(crate) const FEATURES: &[&str] = &[api::features::MOUNTS];

/// Serve a control connection whose first frame was `hello`.
pub(crate) async fn run(shared: Arc<Shared>, peer: Peer, stream: UnixStream, hello: RpcRequest) {
    let (mut rd, mut wr) = stream.into_split();
    let answer = match hello_result(&shared, &hello) {
        Ok(v) => RpcResponse::success(hello.id, v),
        Err(e) => {
            tracing::info!(uid = peer.uid, code = e.code(), "server.hello refused");
            let _ = write_frame(&mut wr, &frame(&failure(Some(hello.id), &e))).await;
            return;
        }
    };
    if write_frame(&mut wr, &frame(&answer)).await.is_err() {
        return;
    }
    tracing::debug!(uid = peer.uid, "control connection open");

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(MAX_REQUESTS_IN_FLIGHT as usize);
    let writer = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if write_frame(&mut wr, &bytes).await.is_err() {
                break;
            }
        }
    });
    let in_flight = Arc::new(Semaphore::new(MAX_REQUESTS_IN_FLIGHT as usize));
    loop {
        let Ok(permit) = Arc::clone(&in_flight).acquire_owned().await else {
            break;
        };
        let payload = match read_frame(&mut rd).await {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => {
                // Out of step (a bad length): answer once and close.
                let _ = tx
                    .send(frame(&failure(None, &ApiError::Protocol(e.into()))))
                    .await;
                break;
            }
        };
        match RpcMessage::from_slice(&payload) {
            Ok(RpcMessage::Request(req)) => {
                let shared = Arc::clone(&shared);
                let tx = tx.clone();
                tokio::spawn(async move {
                    let answer = handle(&shared, peer, req).await;
                    let _ = tx.send(frame(&answer)).await;
                    drop(permit);
                });
            }
            // Notifications and responses from a client carry nothing for the server.
            Ok(_) => {}
            Err(e) => {
                let id = RpcMessage::sniff_id(&payload);
                let _ = tx.send(frame(&failure(id, &e.into()))).await;
            }
        }
    }
    // A lost control connection destroys nothing; requests in flight finish unanswered.
    drop(tx);
    let _ = writer.await;
    tracing::debug!(uid = peer.uid, "control connection closed");
}

fn hello_result(shared: &Shared, req: &RpcRequest) -> Result<Value, ApiError> {
    let p: ServerHelloParams = decode_params(req.params.clone())?;
    p.check_version()?;
    if shared.shutting_down.load(Ordering::SeqCst) {
        return Err(ApiError::ShuttingDown);
    }
    ok(&ServerHelloResult {
        server_version: env!("CARGO_PKG_VERSION").to_owned(),
        api_version: api::API_VERSION.to_owned(),
        features: FEATURES.iter().map(|f| (*f).to_owned()).collect(),
        backend: shared.backend.clone(),
        limits: shared.limits,
    })
}

fn ok<T: Serialize>(v: &T) -> Result<Value, ApiError> {
    serde_json::to_value(v).map_err(|e| ApiError::Protocol(ProtocolError::Internal(e.to_string())))
}

fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError::Protocol(ProtocolError::InvalidParams(msg.into()))
}

fn not_found(id: &str) -> ApiError {
    ApiError::SandboxNotFound(SandboxNotFoundData {
        sandbox_id: id.to_owned(),
    })
}

fn limit(what: &str, limit: u64) -> ApiError {
    ApiError::LimitExceeded(LimitExceededData {
        what: what.to_owned(),
        limit,
    })
}

/// Answer one request on a control connection.
pub(crate) async fn handle(shared: &Shared, peer: Peer, req: RpcRequest) -> RpcResponse {
    let RpcRequest {
        id, method, params, ..
    } = req;
    let out = if shared.shutting_down.load(Ordering::SeqCst) {
        Err(ApiError::ShuttingDown)
    } else {
        call(shared, peer, &method, params).await
    };
    match out {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => {
            tracing::debug!(uid = peer.uid, method = %method, code = e.code(), "request failed");
            failure(Some(id), &e)
        }
    }
}

async fn call(
    shared: &Shared,
    peer: Peer,
    method: &str,
    params: Option<Value>,
) -> Result<Value, ApiError> {
    match method {
        api::METHOD_SERVER_HELLO => Err(ApiError::Protocol(ProtocolError::InvalidRequest(
            "`server.hello` was already answered on this connection".into(),
        ))),
        api::METHOD_SERVER_CAPABILITIES => {
            let _: CapabilitiesParams = decode_params(params)?;
            ok(&capabilities(shared))
        }
        api::METHOD_SANDBOX_CREATE => ok(&create(shared, peer, decode_params(params)?).await?),
        api::METHOD_SANDBOX_GET => {
            let p: SandboxGetParams = decode_params(params)?;
            ok(&info_of(shared, peer, &p.sandbox_id)?)
        }
        api::METHOD_SANDBOX_LIST => ok(&list(shared, peer, decode_params(params)?)?),
        api::METHOD_SANDBOX_SET_TTL => ok(&set_ttl(shared, peer, decode_params(params)?)?),
        api::METHOD_SANDBOX_DESTROY => ok(&destroy(shared, peer, decode_params(params)?).await?),
        api::METHOD_SANDBOX_DESTROY_BY_LABEL => {
            ok(&destroy_by_label(shared, peer, decode_params(params)?).await?)
        }
        api::METHOD_SERVER_SELFTEST
        | api::METHOD_PROCESS_CANCEL
        | api::METHOD_FORWARD_SERVE
        | api::METHOD_FORWARD_REJECT => Err(ApiError::Protocol(ProtocolError::Unimplemented(
            format!("`{method}` is not implemented by this server yet"),
        ))),
        other => Err(ApiError::Protocol(ProtocolError::MethodNotFound(
            other.to_owned(),
        ))),
    }
}

// ---------------------------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------------------------

pub(crate) fn core_mode(m: MountMode) -> sandcrab_sdk::MountMode {
    match m {
        MountMode::ReadOnly => sandcrab_sdk::MountMode::ReadOnly,
        MountMode::ReadWrite => sandcrab_sdk::MountMode::ReadWrite,
    }
}

fn api_mode(m: sandcrab_sdk::MountMode) -> MountMode {
    match m {
        sandcrab_sdk::MountMode::ReadOnly => MountMode::ReadOnly,
        sandcrab_sdk::MountMode::ReadWrite => MountMode::ReadWrite,
    }
}

/// A core error as an API error. `template` names the template of a create.
pub(crate) fn map_err(e: SandboxError, template: &str) -> ApiError {
    let detail = |e: &SandboxError| DetailData {
        detail: e.to_string(),
    };
    let template_error = |kind| {
        ApiError::TemplateError(TemplateErrorData {
            kind,
            template: template.to_owned(),
        })
    };
    match e {
        SandboxError::NotFound {
            kind: "sandbox",
            id,
        } => not_found(&id),
        SandboxError::NotFound {
            kind: "template", ..
        } => template_error(TemplateErrorKind::NotFound),
        SandboxError::Template(ref msg) if msg.contains("npinned") => {
            template_error(TemplateErrorKind::Unpinned)
        }
        SandboxError::Template(_) | SandboxError::DigestMismatch { .. } => {
            template_error(TemplateErrorKind::PinMismatch)
        }
        SandboxError::MountNotAllowed { path, reason, cap } => {
            ApiError::MountNotAllowed(MountNotAllowedData {
                path: path.display().to_string(),
                reason,
                cap: cap.and_then(|c| u32::try_from(c).ok()),
            })
        }
        SandboxError::PolicyUnenforceable(_) | SandboxError::HardwareIsolationRequired(_) => {
            ApiError::PolicyUnenforceable(detail(&e))
        }
        SandboxError::Protocol(p) => ApiError::Protocol(p),
        SandboxError::Unimplemented(what) => {
            ApiError::Protocol(ProtocolError::Unimplemented(what.to_owned()))
        }
        SandboxError::NotFound { .. }
        | SandboxError::AlreadyExists { .. }
        | SandboxError::InvalidConfig(_)
        | SandboxError::Workspace(_)
        | SandboxError::PathEscape(_)
        | SandboxError::WorkspaceRootChanged(_)
        | SandboxError::TooLarge(_)
        | SandboxError::WorkspaceBusy(_) => invalid(e.to_string()),
        SandboxError::Backend { .. }
        | SandboxError::Unavailable(_)
        | SandboxError::Lost(_)
        | SandboxError::Transport(_)
        | SandboxError::Timeout { .. }
        | SandboxError::Io(_) => ApiError::Unavailable(detail(&e)),
    }
}

fn parse_id(id: &str) -> Option<SandboxId> {
    uuid::Uuid::parse_str(id).ok().map(SandboxId)
}

fn parse_time(field: &str, v: Option<&str>) -> Result<Option<DateTime<Utc>>, ApiError> {
    v.map(|s| {
        DateTime::parse_from_rfc3339(s)
            .map(|t| t.with_timezone(&Utc))
            .map_err(|e| invalid(format!("`{field}` is not RFC 3339: {e}")))
    })
    .transpose()
}

/// RFC 3339 in UTC with milliseconds (`2026-01-02T03:04:05.678Z`).
fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn to_info(i: sandcrab_sdk::SandboxInfo) -> SandboxInfo {
    let now = Utc::now();
    SandboxInfo {
        sandbox_id: i.id.to_string(),
        template: i.template,
        labels: i.labels,
        state: SandboxState::Running,
        created_at: rfc3339(i.created_at),
        expires_at: i
            .expires_in
            .and_then(|d| chrono::Duration::from_std(d).ok())
            .and_then(|d| now.checked_add_signed(d))
            .map(rfc3339),
        backend: i.backend.to_string(),
        mounts: i
            .host_mounts
            .into_iter()
            .map(|m| MountSpec {
                path: m.path.display().to_string(),
                mode: api_mode(m.mode),
            })
            .collect(),
        // Counters come with the forwards (next step).
        forwards: i
            .forwards
            .into_iter()
            .map(|f| ForwardInfo {
                guest_port: f.guest_port,
                accepted: 0,
                refused: ForwardRefused::default(),
            })
            .collect(),
        forwards_served: false,
    }
}

// ---------------------------------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------------------------------

fn capabilities(shared: &Shared) -> Capabilities {
    let caps = shared.sandcrab.backend_capabilities();
    // The guest names its tree-kill mode in `hello`. Until the self-test reports it, a live
    // sandbox that fell back to the process group is the evidence.
    let fallback = shared.sandcrab.list_sandboxes().iter().any(|i| {
        shared.sandcrab.manager().get(i.id).is_ok_and(|s| {
            s.hello()
                .features
                .iter()
                .any(|f| f == sandcrab_sdk::protocol::features::TREE_KILL_PROCESS_GROUP)
        })
    });
    Capabilities {
        backend: shared.backend.clone(),
        hardware_isolated: caps.hardware_isolated,
        multi_tenant_safe: caps.multi_tenant_safe,
        supports_host_dir_mounts: caps.supports_host_mounts,
        supports_forwards: caps.supports_forwards,
        tree_kill: if fallback {
            "process_group"
        } else {
            "subreaper"
        }
        .to_owned(),
        nproc_scope: match caps.nproc_scope {
            NprocScope::HostUid => "host_uid",
            NprocScope::Sandbox => "sandbox",
        }
        .to_owned(),
        mount_identity_checked: caps.mount_identity_checked,
        max_mounts: shared.limits.max_mounts,
        selftest: None,
    }
}

/// The `SandboxConfig` of a create, after the server's own rules: the default template and
/// TTL, the TTL ceiling, resource ceilings, and the sandbox user (the request's, else the
/// peer's uid:gid; uid or gid 0 only with `allow_root_user`).
pub(crate) fn sandbox_config(
    shared: &Shared,
    peer: Peer,
    p: SandboxCreate,
) -> Result<SandboxConfig, ApiError> {
    let cfg = &shared.config;
    let template = if p.template.is_empty() {
        cfg.default_template.clone().ok_or_else(|| {
            invalid("`template` is empty and the server has no `default_template`")
        })?
    } else {
        p.template
    };
    let mut c = SandboxConfig::from_template(template);
    c.labels = p.labels;
    c.ttl_secs = match p.ttl_secs {
        None => Some(cfg.limits.default_ttl_secs),
        Some(None) => None,
        Some(Some(0)) => return Err(invalid("`ttl_secs` must be positive (null: no TTL)")),
        Some(Some(t)) if t > cfg.limits.max_ttl_secs => {
            return Err(limit("ttl_secs", cfg.limits.max_ttl_secs));
        }
        Some(t) => t,
    };
    c.host_mounts = p
        .mounts
        .into_iter()
        .map(|m| HostMount::new(m.path, core_mode(m.mode)))
        .collect();
    c = c.network(match p.network {
        NetworkMode::Disabled => NetworkPolicy::Disabled,
        NetworkMode::Unrestricted => NetworkPolicy::Unrestricted,
    });
    c.forwards = p
        .forwards
        .into_iter()
        .map(|f| ForwardSpec {
            guest_port: f.guest_port,
            max_connections: f.max_connections,
        })
        .collect();
    let user = match &p.user {
        Some(u) => u.parse::<UserSpec>().map_err(|e| invalid(e.to_string()))?,
        None => UserSpec::new(peer.uid, peer.gid),
    };
    if (user.uid == 0 || user.gid == 0) && !cfg.allow_root_user {
        return Err(ApiError::PolicyUnenforceable(DetailData {
            detail: format!(
                "sandbox user {user} has uid or gid 0, which needs `allow_root_user = true` in \
                 the server config (name another user with `user`)"
            ),
        }));
    }
    c.user = Some(user);
    let r = p.resources;
    let over = |v: Option<u64>, max: u64| v.is_some_and(|v| v > max);
    if over(r.vcpu.map(u64::from), u64::from(MAX_VCPU)) {
        return Err(limit("vcpu", u64::from(MAX_VCPU)));
    }
    if over(r.memory_mb, MAX_MEMORY_MB) {
        return Err(limit("memory_mb", MAX_MEMORY_MB));
    }
    if over(r.pids_max, MAX_PIDS) {
        return Err(limit("pids_max", MAX_PIDS));
    }
    if over(r.nofile, MAX_NOFILE) {
        return Err(limit("nofile", MAX_NOFILE));
    }
    c.resources = ResourceOverrides {
        vcpu: r.vcpu,
        memory_mb: r.memory_mb,
        disk_mb: r.disk_mb,
        // At most MAX_PIDS, checked above.
        pids_max: r.pids_max.and_then(|v| u32::try_from(v).ok()),
        nofile: r.nofile,
    };
    c.tmp_exec = p.tmp_exec;
    c.tmp_size_mb = p.tmp_size_mb;
    Ok(c)
}

async fn create(shared: &Shared, peer: Peer, p: SandboxCreate) -> Result<SandboxInfo, ApiError> {
    let config = sandbox_config(shared, peer, p)?;
    let template = config.template.clone();
    let slot = shared.reserve_slot()?;
    let sb = shared
        .sandcrab
        .spawn(config)
        .await
        .map_err(|e| map_err(e, &template))?
        .detach();
    drop(slot);
    if shared.shutting_down.load(Ordering::SeqCst) {
        // The stop may have destroyed the others already; do not leave this one behind.
        let _ = sb.destroy().await;
        return Err(ApiError::ShuttingDown);
    }
    shared.set_owner(sb.id(), peer.uid);
    tracing::info!(uid = peer.uid, sandbox = %sb.id(), template = %template, "sandbox created");
    Ok(to_info(sb.info()))
}

/// The sandbox `id` when `peer` owns it.
fn owned(shared: &Shared, peer: Peer, id: &str) -> Result<SandboxId, ApiError> {
    parse_id(id)
        .filter(|sid| shared.owns(peer.uid, *sid))
        .ok_or_else(|| not_found(id))
}

fn info_of(shared: &Shared, peer: Peer, id: &str) -> Result<SandboxInfo, ApiError> {
    let sid = owned(shared, peer, id)?;
    let sb = shared
        .sandcrab
        .manager()
        .get(sid)
        .map_err(|_| not_found(id))?;
    Ok(to_info(sb.info()))
}

fn filter(
    labels: BTreeMap<String, String>,
    after: Option<&str>,
    before: Option<&str>,
) -> Result<SandboxFilter, ApiError> {
    Ok(SandboxFilter {
        labels,
        created_after: parse_time("created_after", after)?,
        created_before: parse_time("created_before", before)?,
    })
}

fn owned_matches(shared: &Shared, peer: Peer, f: &SandboxFilter) -> Vec<sandcrab_sdk::SandboxInfo> {
    shared
        .sandcrab
        .list(f)
        .into_iter()
        .filter(|i| shared.owns(peer.uid, i.id))
        .collect()
}

fn list(shared: &Shared, peer: Peer, p: SandboxListParams) -> Result<Vec<SandboxInfo>, ApiError> {
    let f = filter(
        p.labels,
        p.created_after.as_deref(),
        p.created_before.as_deref(),
    )?;
    shared.prune_owners();
    Ok(owned_matches(shared, peer, &f)
        .into_iter()
        .map(to_info)
        .collect())
}

fn set_ttl(shared: &Shared, peer: Peer, p: SandboxSetTtlParams) -> Result<SandboxInfo, ApiError> {
    let sid = owned(shared, peer, &p.sandbox_id)?;
    let max = shared.config.limits.max_ttl_secs;
    let ttl = match p.ttl_secs {
        Some(0) => return Err(invalid("`ttl_secs` must be positive (null: no TTL)")),
        Some(t) if t > max => return Err(limit("ttl_secs", max)),
        t => t.map(Duration::from_secs),
    };
    shared
        .sandcrab
        .set_ttl(sid, ttl)
        .map_err(|e| map_err(e, ""))?;
    info_of(shared, peer, &p.sandbox_id)
}

async fn destroy(
    shared: &Shared,
    peer: Peer,
    p: SandboxDestroyParams,
) -> Result<SandboxDestroyResult, ApiError> {
    // Another uid's sandbox, or one that is gone: nothing to destroy.
    let Ok(sid) = owned(shared, peer, &p.sandbox_id) else {
        return Ok(SandboxDestroyResult { destroyed: false });
    };
    match shared.sandcrab.manager().destroy(sid).await {
        Ok(_) => {
            shared.forget(sid);
            tracing::info!(uid = peer.uid, sandbox = %sid, "sandbox destroyed");
            Ok(SandboxDestroyResult { destroyed: true })
        }
        Err(SandboxError::NotFound { .. }) => {
            shared.forget(sid);
            Ok(SandboxDestroyResult { destroyed: false })
        }
        Err(e) => Err(map_err(e, "")),
    }
}

/// Design §7, within the peer's own sandboxes: destroy every match, then look again.
async fn destroy_by_label(
    shared: &Shared,
    peer: Peer,
    p: DestroyByLabelParams,
) -> Result<DestroyByLabelResult, ApiError> {
    if p.labels.is_empty() {
        return Err(invalid("`labels` must not be empty"));
    }
    sandcrab_sdk::core::validate_labels(&p.labels).map_err(|e| map_err(e, ""))?;
    let f = filter(p.labels, None, None)?;
    let matched: Vec<SandboxId> = owned_matches(shared, peer, &f)
        .into_iter()
        .map(|i| i.id)
        .collect();
    let mut tasks = tokio::task::JoinSet::new();
    for id in matched.iter().copied() {
        let manager = shared.sandcrab.manager().clone();
        tasks.spawn(async move { (id, manager.destroy(id).await.is_ok()) });
    }
    let mut destroyed = 0u64;
    while let Some(r) = tasks.join_next().await {
        if let Ok((id, true)) = r {
            shared.forget(id);
            destroyed += 1;
        }
    }
    let remaining: Vec<String> = owned_matches(shared, peer, &f)
        .into_iter()
        .map(|i| i.id.to_string())
        .collect();
    tracing::info!(
        uid = peer.uid,
        matched = matched.len(),
        destroyed,
        "destroy by label"
    );
    Ok(DestroyByLabelResult {
        matched: matched.len() as u64,
        destroyed,
        empty: destroyed == matched.len() as u64 && remaining.is_empty(),
        remaining,
    })
}

#[cfg(test)]
mod tests {
    use sandcrab_sdk::Sandcrab;
    use sandcrab_sdk::core::testing::FakeBackend;
    use sandcrab_sdk::protocol::{ResponsePayload, codes};
    use serde_json::json;

    use super::*;
    use crate::ServerConfig;

    fn shared(tmp: &std::path::Path) -> Shared {
        let d = tmp.join("templates/base");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("sandcrab.toml"),
            "[template]\nname = \"base\"\nversion = \"0.1.0\"\nimage = \"sandcrab/base:0.1.0\"\n\
             [resources]\nvcpu = 1\nmemory_mb = 512\ndisk_mb = 2048\n",
        )
        .unwrap();
        std::fs::write(d.join("Dockerfile"), "FROM scratch\n").unwrap();
        let sandcrab = Sandcrab::builder()
            .backend(FakeBackend::shared_kernel())
            .state_dir(tmp.join("state"))
            .templates_dir(tmp.join("templates"))
            .build()
            .unwrap();
        Shared::new(sandcrab, ServerConfig::default(), vec![1000, 2000])
    }

    async fn call(s: &Shared, uid: u32, method: &str, params: Value) -> Result<Value, i64> {
        let req = RpcRequest::new(1, method, Some(params));
        match handle(s, Peer { uid, gid: uid }, req).await.payload {
            ResponsePayload::Result(v) => Ok(v),
            ResponsePayload::Error(e) => Err(e.code),
        }
    }

    /// A uid sees and touches only the sandboxes it created.
    #[tokio::test]
    async fn each_uid_sees_and_touches_only_its_own_sandboxes() {
        let tmp = tempfile::tempdir().unwrap();
        let s = shared(tmp.path());
        let labels = json!({"template": "base", "labels": {"run": "r1"}});
        let a = call(&s, 1000, "sandbox.create", labels.clone())
            .await
            .unwrap();
        let b = call(&s, 2000, "sandbox.create", labels).await.unwrap();
        let (id_a, id_b) = (a["sandbox_id"].clone(), b["sandbox_id"].clone());

        let listed = |v: Value| v.as_array().unwrap().len();
        assert_eq!(
            listed(call(&s, 1000, "sandbox.list", json!({})).await.unwrap()),
            1
        );
        assert_eq!(
            listed(call(&s, 2000, "sandbox.list", json!({})).await.unwrap()),
            1
        );
        let nf = Err(api::codes::SANDBOX_NOT_FOUND);
        assert_eq!(
            call(&s, 2000, "sandbox.get", json!({"sandbox_id": id_a})).await,
            nf
        );
        let ttl = json!({"sandbox_id": id_a, "ttl_secs": 60});
        assert_eq!(call(&s, 2000, "sandbox.set_ttl", ttl).await, nf);
        assert_eq!(
            call(&s, 2000, "sandbox.destroy", json!({"sandbox_id": id_a})).await,
            Ok(json!({"destroyed": false}))
        );
        // destroy_by_label stays within the caller's sandboxes.
        let r = call(
            &s,
            2000,
            "sandbox.destroy_by_label",
            json!({"labels": {"run": "r1"}}),
        )
        .await
        .unwrap();
        assert_eq!(
            (r["matched"].clone(), r["empty"].clone()),
            (json!(1), json!(true))
        );
        assert_eq!(
            call(&s, 1000, "sandbox.get", json!({"sandbox_id": id_a}))
                .await
                .unwrap()["sandbox_id"],
            id_a
        );
        assert_eq!(
            call(&s, 2000, "sandbox.get", json!({"sandbox_id": id_b})).await,
            nf
        );
        assert_eq!(
            call(&s, 1000, "sandbox.destroy", json!({"sandbox_id": id_a})).await,
            Ok(json!({"destroyed": true}))
        );
    }

    #[tokio::test]
    async fn requests_during_shutdown_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let s = shared(tmp.path());
        s.shutting_down.store(true, Ordering::SeqCst);
        let e = call(&s, 1000, "sandbox.create", json!({"template": "base"})).await;
        assert_eq!(e, Err(api::codes::SHUTTING_DOWN));
        assert!(s.sandcrab.list_sandboxes().is_empty());
    }

    #[test]
    fn errors_map_to_api_codes() {
        let m = |e| map_err(e, "base").code();
        assert_eq!(
            m(SandboxError::Unavailable("x".into())),
            api::codes::UNAVAILABLE
        );
        assert_eq!(
            m(SandboxError::Backend {
                backend: "docker".into(),
                message: "down".into()
            }),
            api::codes::UNAVAILABLE
        );
        assert_eq!(
            m(SandboxError::InvalidConfig("x".into())),
            codes::INVALID_PARAMS
        );
        assert_eq!(
            m(SandboxError::PolicyUnenforceable("x".into())),
            api::codes::POLICY_UNENFORCEABLE
        );
        assert_eq!(
            m(SandboxError::Template("UnpinnedImage: no pin".into())),
            api::codes::TEMPLATE_ERROR
        );
        let e = map_err(SandboxError::Template("UnpinnedImage".into()), "base");
        assert!(matches!(
            e,
            ApiError::TemplateError(TemplateErrorData {
                kind: TemplateErrorKind::Unpinned,
                ..
            })
        ));
        assert_eq!(
            m(SandboxError::NotFound {
                kind: "sandbox",
                id: "x".into()
            }),
            api::codes::SANDBOX_NOT_FOUND
        );
    }
}
