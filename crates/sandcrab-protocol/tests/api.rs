//! Server API types: round trips, unknown fields, wire names, errors and the version rule.

use std::collections::BTreeMap;
use std::fmt::Debug;

use sandcrab_protocol::api::*;
use sandcrab_protocol::{
    ForwardRefused, ProcessCancelParams, ProcessCancelResult, ProcessOpenParams, ProtocolError,
    RpcError, RpcMessage, StdinMode, encode_frame,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

const PID: &str = "6f1c2a43-8a51-4c43-9d3e-2b0a8e9c1f00";

fn labels() -> BTreeMap<String, String> {
    BTreeMap::from([("run".into(), "r-1".into()), ("group".into(), "g".into())])
}

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(v: &T) -> Value {
    let json = serde_json::to_value(v).unwrap();
    let back: T = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(&back, v, "{json}");
    json
}

/// A params type: round-trips through a request frame and rejects an unknown field.
fn params<T: Serialize + DeserializeOwned + PartialEq + Debug>(method: &str, v: &T) {
    let frame = encode_frame(&request(1, method, v)).unwrap();
    let RpcMessage::Request(req) = RpcMessage::from_slice(&frame[4..]).unwrap() else {
        panic!("request")
    };
    assert_eq!(req.method, method);
    assert_eq!(&decode_params::<T>(req.params.clone()).unwrap(), v);
    let mut extra = req.params.unwrap();
    extra["unknown_field"] = json!(1);
    assert!(
        matches!(
            decode_params::<T>(Some(extra)),
            Err(ProtocolError::InvalidParams(_))
        ),
        "{method} must reject unknown fields"
    );
}

/// A result or notification type: round-trips and accepts an unknown field.
fn result<T: Serialize + DeserializeOwned + PartialEq + Debug>(v: &T) {
    let mut json = round_trip(v);
    if let Value::Object(m) = &mut json {
        m.insert("unknown_field".into(), json!(1));
        assert_eq!(&serde_json::from_value::<T>(json).unwrap(), v);
    }
}

fn info() -> SandboxInfo {
    SandboxInfo {
        sandbox_id: "sb-1".into(),
        template: "base".into(),
        labels: labels(),
        state: SandboxState::Running,
        created_at: "2026-11-26T10:00:00Z".into(),
        expires_at: None,
        backend: "docker".into(),
        mounts: vec![MountSpec {
            path: "/srv/app-data/run-1".into(),
            mode: MountMode::ReadWrite,
        }],
        forwards: vec![ForwardInfo {
            guest_port: 5432,
            accepted: 3,
            refused: ForwardRefused {
                no_listener: 0,
                cap: 1,
                unclaimed: 2,
                rejected: 3,
            },
        }],
        forwards_served: true,
    }
}

fn full_create() -> SandboxCreate {
    SandboxCreate {
        template: "base".into(),
        labels: labels(),
        ttl_secs: Some(Some(600)),
        mounts: vec![
            MountSpec {
                path: "/srv/a".into(),
                mode: MountMode::ReadOnly,
            },
            MountSpec {
                path: "/srv/b".into(),
                mode: MountMode::ReadWrite,
            },
        ],
        network: NetworkMode::Unrestricted,
        forwards: vec![
            ForwardSpec {
                guest_port: 8080,
                max_connections: Some(4),
            },
            ForwardSpec {
                guest_port: 5432,
                max_connections: None,
            },
        ],
        user: Some("1000:1000".into()),
        resources: Resources {
            vcpu: Some(2),
            memory_mb: Some(2048),
            disk_mb: Some(4096),
            pids_max: Some(512),
            nofile: Some(4096),
        },
        tmp_exec: true,
        tmp_size_mb: Some(512),
    }
}

fn spawn() -> ProcessSpawnParams {
    let mut open = ProcessOpenParams::new(PID, vec!["node".into(), "x.js".into()]);
    open.env.insert("PATH".into(), "/usr/bin".into());
    open.stdin = StdinMode::Pipe;
    open.timeout_ms = Some(1000);
    ProcessSpawnParams {
        sandbox_id: "sb-1".into(),
        open,
    }
}

#[test]
fn every_params_type_round_trips_and_rejects_unknown_fields() {
    params(
        METHOD_SERVER_HELLO,
        &ServerHelloParams {
            client: "test".into(),
            api_version: API_VERSION.into(),
        },
    );
    params(METHOD_SERVER_CAPABILITIES, &CapabilitiesParams::default());
    params(
        METHOD_SERVER_CAPABILITIES,
        &CapabilitiesParams {
            template: Some("base".into()),
        },
    );
    params(
        METHOD_SERVER_SELFTEST,
        &SelftestParams {
            template: "base".into(),
            max_age_secs: Some(60),
        },
    );
    params(METHOD_SANDBOX_CREATE, &full_create());
    params(METHOD_SANDBOX_CREATE, &SandboxCreate::new("base"));
    params(
        METHOD_SANDBOX_GET,
        &SandboxGetParams {
            sandbox_id: "sb-1".into(),
        },
    );
    params(METHOD_SANDBOX_LIST, &SandboxListParams::default());
    params(
        METHOD_SANDBOX_LIST,
        &SandboxListParams {
            labels: labels(),
            created_after: Some("2026-11-26T10:00:00Z".into()),
            created_before: Some("2026-11-27T10:00:00+01:00".into()),
        },
    );
    for ttl in [Some(60), None] {
        params(
            METHOD_SANDBOX_SET_TTL,
            &SandboxSetTtlParams {
                sandbox_id: "sb-1".into(),
                ttl_secs: ttl,
            },
        );
    }
    params(
        METHOD_SANDBOX_DESTROY,
        &SandboxDestroyParams {
            sandbox_id: "sb-1".into(),
        },
    );
    params(
        METHOD_SANDBOX_DESTROY_BY_LABEL,
        &DestroyByLabelParams { labels: labels() },
    );
    params(
        METHOD_PROCESS_CANCEL,
        &ProcessCancelParams {
            process_id: PID.into(),
            grace_ms: Some(0),
        },
    );
    params(
        METHOD_FORWARD_SERVE,
        &ForwardServeParams {
            sandbox_id: "sb-1".into(),
        },
    );
    params(
        METHOD_FORWARD_REJECT,
        &ForwardRejectParams {
            conn_id: "00ff".into(),
            reason: Some("ECONNREFUSED".into()),
        },
    );
    params(METHOD_PROCESS_SPAWN, &spawn());
    params(
        METHOD_PROCESS_SPAWN,
        &ProcessSpawnParams {
            sandbox_id: "sb-1".into(),
            open: ProcessOpenParams::new(PID, vec!["true".into()]),
        },
    );
    params(
        METHOD_FORWARD_ATTACH,
        &ForwardAttachParams {
            conn_id: "00ff".into(),
        },
    );
}

#[test]
fn nested_params_types_reject_unknown_fields() {
    let base = serde_json::to_value(full_create()).unwrap();
    for (path, value) in [
        ("/mounts/0", json!({"path": "/a", "mode": "ro", "x": 1})),
        ("/forwards/0", json!({"guest_port": 1, "x": 1})),
        ("/resources", json!({"cpu": 1})),
        ("/mounts/0/mode", json!("readonly")),
        ("/network", json!("bridge")),
    ] {
        let mut v = base.clone();
        *v.pointer_mut(path).unwrap() = value;
        assert!(
            decode_params::<SandboxCreate>(Some(v)).is_err(),
            "{path} must be rejected"
        );
    }
}

#[test]
fn every_result_and_notification_round_trips_and_accepts_unknown_fields() {
    result(&ServerHelloResult {
        server_version: "0.1.0".into(),
        api_version: API_VERSION.into(),
        features: features::ALL.iter().map(|s| (*s).to_owned()).collect(),
        backend: "docker".into(),
        limits: ServerLimits {
            max_connections: 1024,
            max_requests_in_flight: 64,
            max_sandboxes: 64,
            max_processes_per_sandbox: 64,
            max_mounts: 16,
            max_vcpu: 8,
            max_memory_mb: 16384,
            max_pids: 8192,
            max_nofile: 1_048_576,
            default_ttl_secs: 1800,
            max_ttl_secs: 86400,
        },
    });
    let summary = SelftestSummary {
        template: "base".into(),
        ok: false,
        reason: Some("net-external-refused was inconclusive".into()),
        age_secs: 12,
    };
    for selftest in [None, Some(summary)] {
        result(&Capabilities {
            backend: "docker".into(),
            hardware_isolated: false,
            multi_tenant_safe: false,
            supports_host_dir_mounts: true,
            supports_forwards: true,
            tree_kill: "subreaper".into(),
            nproc_scope: "host_uid".into(),
            mount_identity_checked: true,
            max_mounts: 16,
            selftest,
        });
    }
    result(&SelftestReport {
        ok: false,
        reason: Some("tree-kill failed".into()),
        backend: "libkrun".into(),
        template: "base".into(),
        started_at: "2026-11-26T10:00:00Z".into(),
        duration_ms: 4200,
        lines: vec![
            SelftestLine {
                id: "caps-empty".into(),
                status: SelftestStatus::Pass,
                required: true,
                detail: "all capability sets are empty".into(),
                evidence: json!({"CapEff": "0000000000000000"}),
            },
            SelftestLine {
                id: "dns-refused".into(),
                status: SelftestStatus::Inconclusive,
                required: true,
                detail: "timed out".into(),
                evidence: json!({"tried": "udp 1.1.1.1:53", "errno": null}),
            },
        ],
    });
    let mut ttl = info();
    ttl.expires_at = Some("2026-11-26T10:30:00Z".into());
    ttl.state = SandboxState::Destroying;
    result(&info());
    result(&ttl);
    round_trip(&SandboxList(vec![info(), ttl]));
    result(&SandboxDestroyResult { destroyed: true });
    result(&DestroyByLabelResult {
        matched: 3,
        destroyed: 2,
        remaining: vec!["sb-3".into()],
        empty: false,
    });
    result(&ProcessCancelResult {
        was_running: true,
        tree_clean: true,
    });
    result(&ForwardServeResult {
        ports: vec![5432, 8080],
    });
    result(&Empty {});
    result(&ProcessSpawnAck {
        process_id: PID.into(),
    });
    result(&ForwardIncoming {
        conn_id: "0123456789abcdef0123456789abcdef".into(),
        sandbox_id: "sb-1".into(),
        guest_port: 5432,
    });
    for reason in [
        DestroyReason::Destroyed,
        DestroyReason::Ttl,
        DestroyReason::BackendExit,
    ] {
        result(&SandboxDestroyed {
            sandbox_id: "sb-1".into(),
            reason,
        });
    }
    result(&ServerShutdown { grace_ms: 5000 });
}

#[test]
fn wire_names() {
    let v = serde_json::to_value(full_create()).unwrap();
    assert_eq!(v["mounts"][0]["mode"], "ro");
    assert_eq!(v["mounts"][1]["mode"], "rw");
    assert_eq!(v["network"], "unrestricted");
    assert_eq!(
        serde_json::to_value(NetworkMode::default()).unwrap(),
        "disabled"
    );
    let v = serde_json::to_value(info()).unwrap();
    assert_eq!(v["state"], "running");
    assert_eq!(v["expires_at"], Value::Null);
    assert_eq!(
        v["forwards"][0]["refused"],
        json!({"no_listener": 0, "cap": 1, "unclaimed": 2, "rejected": 3})
    );
    assert_eq!(
        serde_json::to_value(DestroyReason::BackendExit).unwrap(),
        "backend_exit"
    );
    assert_eq!(
        serde_json::to_value(SelftestStatus::Inconclusive).unwrap(),
        "inconclusive"
    );
    assert_eq!(
        serde_json::to_value(TemplateErrorKind::PinMismatch).unwrap(),
        "pin_mismatch"
    );
    assert_eq!(
        serde_json::to_value(SandboxList::default()).unwrap(),
        json!([])
    );
}

#[test]
fn create_defaults_and_the_ttl_tri_state() {
    let c: SandboxCreate = decode_params(Some(json!({"template": "base"}))).unwrap();
    assert_eq!(c, SandboxCreate::new("base"));
    assert_eq!(c.network, NetworkMode::Disabled);
    assert_eq!(
        serde_json::to_value(&c).unwrap(),
        json!({"template": "base", "labels": {}, "mounts": [], "network": "disabled",
               "forwards": [], "resources": {}, "tmp_exec": false})
    );

    // Absent: the server's default. null: no TTL. A number: that TTL.
    let no_ttl: SandboxCreate =
        decode_params(Some(json!({"template": "base", "ttl_secs": null}))).unwrap();
    assert_eq!(no_ttl.ttl_secs, Some(None));
    assert_eq!(
        serde_json::to_value(&no_ttl).unwrap()["ttl_secs"],
        Value::Null
    );
    let ttl: SandboxCreate =
        decode_params(Some(json!({"template": "base", "ttl_secs": 60}))).unwrap();
    assert_eq!(ttl.ttl_secs, Some(Some(60)));
    assert!(decode_params::<SandboxCreate>(Some(json!({}))).is_err());
}

#[test]
fn set_ttl_needs_the_field_even_when_null() {
    let p: SandboxSetTtlParams =
        decode_params(Some(json!({"sandbox_id": "s", "ttl_secs": null}))).unwrap();
    assert_eq!(p.ttl_secs, None);
    assert!(decode_params::<SandboxSetTtlParams>(Some(json!({"sandbox_id": "s"}))).is_err());
    assert_eq!(
        serde_json::to_value(&p).unwrap(),
        json!({"sandbox_id": "s", "ttl_secs": null})
    );
}

#[test]
fn spawn_params_are_flat_and_reuse_process_open() {
    let s = spawn();
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["sandbox_id"], "sb-1");
    assert_eq!(v["process_id"], PID);
    assert_eq!(v["argv"], json!(["node", "x.js"]));
    assert_eq!(v["stdin"], "pipe");
    assert!(v.get("open").is_none());

    let req = s.to_request(7);
    assert_eq!(ProcessSpawnParams::from_request(&req).unwrap(), s);

    let mut bad = v.clone();
    bad["argv"] = json!([]);
    let req = request(1, METHOD_PROCESS_SPAWN, &bad);
    assert!(matches!(
        ProcessSpawnParams::from_request(&req),
        Err(ProtocolError::InvalidParams(_))
    ));
    for drop in ["sandbox_id", "process_id"] {
        let mut v = v.clone();
        v.as_object_mut().unwrap().remove(drop);
        assert!(
            decode_params::<ProcessSpawnParams>(Some(v)).is_err(),
            "{drop}"
        );
    }
    let mut v2 = v.clone();
    v2["sandbox_id"] = json!(1);
    assert!(decode_params::<ProcessSpawnParams>(Some(v2)).is_err());
    let mut v3 = v.clone();
    v3["sandbox_id"] = json!("");
    let req = request(1, METHOD_PROCESS_SPAWN, &v3);
    assert!(ProcessSpawnParams::from_request(&req).is_err());
    let other = request(1, METHOD_SANDBOX_GET, &v);
    assert!(matches!(
        ProcessSpawnParams::from_request(&other),
        Err(ProtocolError::MethodNotFound(_))
    ));
}

#[test]
fn notifications_round_trip_through_a_frame() {
    let n = ForwardIncoming {
        conn_id: "ab".into(),
        sandbox_id: "sb".into(),
        guest_port: 1,
    };
    let frame = encode_frame(&notification(NOTIFY_FORWARD_INCOMING, &n)).unwrap();
    let RpcMessage::Notification(got) = RpcMessage::from_slice(&frame[4..]).unwrap() else {
        panic!("notification")
    };
    assert_eq!(got.method, NOTIFY_FORWARD_INCOMING);
    assert_eq!(
        serde_json::from_value::<ForwardIncoming>(got.params.unwrap()).unwrap(),
        n
    );
}

fn all_errors() -> Vec<(ApiError, i64)> {
    let detail = || DetailData {
        detail: "docker daemon down".into(),
    };
    vec![
        (ApiError::Unavailable(detail()), -32101),
        (
            ApiError::SandboxNotFound(SandboxNotFoundData {
                sandbox_id: "sb".into(),
            }),
            -32102,
        ),
        (
            ApiError::MountNotAllowed(MountNotAllowedData {
                path: "/srv".into(),
                reason: "too many mounts".into(),
                cap: Some(16),
            }),
            -32103,
        ),
        (
            ApiError::MountNotAllowed(MountNotAllowedData {
                path: "/etc".into(),
                reason: "outside the allowed roots".into(),
                cap: None,
            }),
            -32103,
        ),
        (
            ApiError::LimitExceeded(LimitExceededData {
                what: "sandboxes".into(),
                limit: 64,
            }),
            -32104,
        ),
        (ApiError::PolicyUnenforceable(detail()), -32105),
        (
            ApiError::TemplateError(TemplateErrorData {
                kind: TemplateErrorKind::Unpinned,
                template: "base".into(),
            }),
            -32106,
        ),
        (ApiError::ShuttingDown, -32107),
        (ApiError::Conflict(detail()), -32108),
        (
            ApiError::UnsupportedVersion(UnsupportedVersionData {
                server: "1.0".into(),
                client: "2.0".into(),
            }),
            -32009,
        ),
        (ApiError::Unauthorized, -32008),
        (
            ApiError::Protocol(ProtocolError::SpawnFailed {
                errno: "ENOENT".into(),
                what: sandcrab_protocol::process::SpawnTarget::Program,
            }),
            -32010,
        ),
        (
            ApiError::Protocol(ProtocolError::LimitExceeded {
                what: "processes".into(),
                limit: 64,
            }),
            -32011,
        ),
        (
            ApiError::Protocol(ProtocolError::Cancelled("x".into())),
            -32012,
        ),
        (
            ApiError::Protocol(ProtocolError::InvalidParams("x".into())),
            -32602,
        ),
    ]
}

#[test]
fn errors_map_to_codes_and_back() {
    for (err, code) in all_errors() {
        assert_eq!(err.code(), code, "{err}");
        let rpc = err.to_rpc_error();
        assert_eq!(rpc.code, code);
        // Through the wire.
        let rpc: RpcError = serde_json::from_value(serde_json::to_value(&rpc).unwrap()).unwrap();
        assert_eq!(ApiError::from_rpc_error(rpc), err);
    }
    let rpc = ApiError::ShuttingDown.to_rpc_error();
    assert_eq!(rpc.data, Some(json!({})));
    let rpc = ApiError::MountNotAllowed(MountNotAllowedData {
        path: "/a".into(),
        reason: "nested".into(),
        cap: None,
    })
    .to_rpc_error();
    assert_eq!(rpc.data, Some(json!({"path": "/a", "reason": "nested"})));
}

#[test]
fn malformed_server_error_data_is_kept_verbatim() {
    let rpc = RpcError {
        code: codes::SANDBOX_NOT_FOUND,
        message: "gone".into(),
        data: Some(json!({"id": 1})),
    };
    assert!(matches!(
        ApiError::from_rpc_error(rpc),
        ApiError::Protocol(ProtocolError::Other { code: -32102, .. })
    ));
    // crabd's own shapes of the shared codes stay crabd's.
    let crabd = ProtocolError::UnsupportedVersion {
        crabd: "0.3".into(),
        host: "0.2".into(),
    };
    assert_eq!(
        ApiError::from_rpc_error(crabd.to_rpc_error()),
        ApiError::Protocol(crabd)
    );
    let crabd = ProtocolError::Unauthorized("bad token".into());
    assert_eq!(
        ApiError::from_rpc_error(crabd.to_rpc_error()),
        ApiError::Protocol(crabd)
    );
}

#[test]
fn version_rule() {
    let hello = |v: &str| ServerHelloParams {
        client: "t".into(),
        api_version: v.into(),
    };
    let (major, minor) = API_VERSION.split_once('.').unwrap();
    let minor: u32 = minor.parse().unwrap();
    hello(API_VERSION).check_version().unwrap();
    // A newer minor is served: the client sends newer fields only when `features` names them.
    hello(&format!("{major}.{}", minor + 1))
        .check_version()
        .unwrap();
    if minor > 0 {
        hello(&format!("{major}.{}", minor - 1))
            .check_version()
            .unwrap();
    }
    for v in [
        format!("{}.0", major.parse::<u32>().unwrap() + 1),
        "0.9".to_owned(),
        "1".to_owned(),
        "1.x".to_owned(),
        "1.0.0".to_owned(),
        String::new(),
        "+1.0".to_owned(),
    ] {
        let err = hello(&v).check_version().unwrap_err();
        assert_eq!(
            err,
            ApiError::UnsupportedVersion(UnsupportedVersionData {
                server: API_VERSION.into(),
                client: v.clone(),
            }),
            "{v}"
        );
    }
}
