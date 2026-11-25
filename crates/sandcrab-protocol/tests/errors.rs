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
