use sandcrab_protocol::*;
use serde_json::json;

fn all_errors() -> Vec<(ProtocolError, i64)> {
    vec![
        (ProtocolError::Parse("x".into()), -32700),
        (ProtocolError::InvalidRequest("x".into()), -32600),
        (ProtocolError::MethodNotFound("x".into()), -32601),
        (ProtocolError::InvalidParams("x".into()), -32602),
        (ProtocolError::Internal("x".into()), -32603),
        (ProtocolError::ExecutionFailed("x".into()), -32001),
        (ProtocolError::TimedOut { timeout_ms: 500 }, -32002),
        (ProtocolError::PathNotAllowed("/etc".into()), -32003),
        (ProtocolError::NotFound("a.txt".into()), -32004),
        (
            ProtocolError::AmbiguousEdit {
                path: "a.txt".into(),
                occurrences: 3,
            },
            -32005,
        ),
        (ProtocolError::TooLarge { size: 10, limit: 5 }, -32006),
        (ProtocolError::Unimplemented("x".into()), -32007),
        (ProtocolError::Unauthorized("x".into()), -32008),
        (
            ProtocolError::UnsupportedVersion {
                crabd: "0.3".into(),
                host: "0.2".into(),
            },
            -32009,
        ),
        (
            ProtocolError::SpawnFailed {
                errno: "ENOENT".into(),
                what: SpawnTarget::Program,
            },
            -32010,
        ),
        (
            ProtocolError::SpawnFailed {
                errno: "ENOTDIR".into(),
                what: SpawnTarget::Cwd,
            },
            -32010,
        ),
        (
            ProtocolError::LimitExceeded {
                what: "processes".into(),
                limit: 64,
            },
            -32011,
        ),
        (ProtocolError::Cancelled("p".into()), -32012),
        (
            ProtocolError::Other {
                code: -31000,
                message: "custom".into(),
                data: Some(json!([1])),
            },
            -31000,
        ),
    ]
}

#[test]
fn codes_and_rpc_error_round_trip() {
    for (err, code) in all_errors() {
        assert_eq!(err.code(), code, "{err:?}");
        let rpc = err.to_rpc_error();
        assert_eq!(rpc.code, code);
        assert_eq!(ProtocolError::from(rpc), err);

        // ProtocolError itself serializes as a JSON-RPC error object.
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["code"], code);
        assert_eq!(serde_json::from_value::<ProtocolError>(v).unwrap(), err);
    }
}

#[test]
fn structured_data_is_exposed() {
    let rpc = ProtocolError::AmbiguousEdit {
        path: "a.txt".into(),
        occurrences: 2,
    }
    .to_rpc_error();
    assert_eq!(rpc.data, Some(json!({"path": "a.txt", "occurrences": 2})));
    assert!(rpc.message.contains("replace_all"));
}

#[test]
fn foreign_errors_degrade_gracefully() {
    // Known code without our `data`: message becomes the detail.
    let e = ProtocolError::from(RpcError {
        code: -32601,
        message: "Method not found".into(),
        data: None,
    });
    assert_eq!(e, ProtocolError::MethodNotFound("Method not found".into()));

    // Structured code with malformed data: preserved as Other.
    let rpc = RpcError {
        code: -32002,
        message: "timeout".into(),
        data: None,
    };
    assert_eq!(
        ProtocolError::from(rpc.clone()),
        ProtocolError::Other {
            code: rpc.code,
            message: rpc.message,
            data: None
        }
    );
}

#[test]
fn protocol_0_3_error_data() {
    assert_eq!(codes::UNSUPPORTED_VERSION, -32009);
    assert_eq!(codes::SPAWN_FAILED, -32010);
    assert_eq!(codes::LIMIT_EXCEEDED, -32011);
    assert_eq!(codes::CANCELLED, -32012);

    let rpc = ProtocolError::SpawnFailed {
        errno: "EACCES".into(),
        what: SpawnTarget::Program,
    }
    .to_rpc_error();
    assert_eq!(
        rpc.data,
        Some(json!({"errno": "EACCES", "what": "program"}))
    );
    assert_eq!(rpc.message, "could not start the program: EACCES");

    let rpc = ProtocolError::LimitExceeded {
        what: "processes".into(),
        limit: 64,
    }
    .to_rpc_error();
    assert_eq!(rpc.data, Some(json!({"what": "processes", "limit": 64})));

    let rpc = ProtocolError::UnsupportedVersion {
        crabd: "0.3".into(),
        host: "0.2".into(),
    }
    .to_rpc_error();
    assert_eq!(rpc.data, Some(json!({"crabd": "0.3", "host": "0.2"})));
    assert!(rpc.message.contains("0.3") && rpc.message.contains("0.2"));

    // Malformed data degrades to Other instead of guessing.
    for (code, data) in [
        (-32009, json!({"crabd": "0.3"})),
        (-32010, json!({"errno": "ENOENT", "what": "kernel"})),
        (-32010, json!({"what": "program"})),
        (-32011, json!({"what": "processes"})),
    ] {
        let e = ProtocolError::from(RpcError {
            code,
            message: "m".into(),
            data: Some(data),
        });
        assert!(matches!(e, ProtocolError::Other { .. }), "{code}: {e:?}");
    }
}
