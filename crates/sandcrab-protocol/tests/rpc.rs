use std::collections::BTreeMap;

use sandcrab_protocol::*;
use serde_json::{Value, json};

fn all_calls() -> Vec<ToolCall> {
    let env: BTreeMap<String, String> = [("K".to_owned(), "V".to_owned())].into();
    vec![
        ToolCall::Bash(BashParams {
            command: "ls -la".into(),
            cwd: Some("/workspace".into()),
            env: env.clone(),
            timeout_ms: Some(5_000),
        }),
        ToolCall::FileRead(FileReadParams {
            path: "a.txt".into(),
            offset: Some(10),
            limit: Some(20),
        }),
        ToolCall::FileWrite(FileWriteParams {
            path: "a.txt".into(),
            content: "hi\n".into(),
            append: true,
        }),
        ToolCall::FileEdit(FileEditParams {
            path: "a.txt".into(),
            old_string: "hi".into(),
            new_string: "bye".into(),
            replace_all: false,
        }),
        ToolCall::FileDelete(FileDeleteParams {
            path: "dir".into(),
            recursive: true,
        }),
        ToolCall::FileMove(FileMoveParams {
            from: "a".into(),
            to: "b".into(),
            overwrite: true,
        }),
        ToolCall::ListDir(ListDirParams {
            path: None,
            depth: Some(2),
            respect_gitignore: false,
        }),
        ToolCall::Glob(GlobParams {
            pattern: "**/*.rs".into(),
            root: Some("src".into()),
            respect_gitignore: true,
        }),
        ToolCall::Grep(GrepParams {
            pattern: "fn main".into(),
            root: None,
            glob: Some("*.rs".into()),
            literal: true,
            case_insensitive: true,
            context_lines: Some(2),
        }),
        ToolCall::ProcessStart(ProcessStartParams {
            command: "npm run dev".into(),
            cwd: None,
            env,
            pty: true,
        }),
        ToolCall::ProcessPoll(ProcessPollParams {
            process_id: "p1".into(),
        }),
        ToolCall::ProcessStdin(ProcessStdinParams {
            process_id: "p1".into(),
            data: "q\n".into(),
        }),
        ToolCall::ProcessKill(ProcessKillParams {
            process_id: "p1".into(),
            signal: KillSignal::Kill,
        }),
        ToolCall::ProcessList(ProcessListParams {}),
        ToolCall::Upload(UploadParams {
            path: "img.png".into(),
            content_base64: "aGk=".into(),
            mode: Some(0o644),
            overwrite: true,
        }),
        ToolCall::Download(DownloadParams {
            path: "img.png".into(),
            max_bytes: Some(1024),
        }),
    ]
}

fn all_results() -> Vec<ToolResult> {
    vec![
        BashResult {
            exit_code: None,
            signal: Some(9),
            stdout: "out".into(),
            stderr: "err".into(),
            truncated: true,
            timed_out: true,
            duration_ms: 1234,
        }
        .into(),
        FileReadResult {
            content: "     1\thi".into(),
            total_lines: 1,
            start_line: 1,
            end_line: 1,
            truncated: false,
            is_binary: false,
        }
        .into(),
        FileWriteResult {
            path: "/w/a".into(),
            bytes_written: 3,
            created: true,
        }
        .into(),
        FileEditResult {
            replacements: 1,
            diff: "-hi\n+bye\n".into(),
        }
        .into(),
        FileDeleteResult {
            path: "/w/dir".into(),
            kind: EntryKind::Dir,
        }
        .into(),
        FileMoveResult {
            from: "/w/a".into(),
            to: "/w/b".into(),
        }
        .into(),
        ListDirResult {
            entries: vec![DirEntry {
                path: "src/lib.rs".into(),
                kind: EntryKind::File,
                size: 42,
                mode: 0o644,
            }],
            truncated: false,
        }
        .into(),
        GlobResult {
            paths: vec!["src/lib.rs".into()],
            truncated: true,
        }
        .into(),
        GrepResult {
            matches: vec![GrepMatch {
                path: "src/main.rs".into(),
                line_number: 3,
                line: "fn main() {".into(),
                context_before: vec!["".into()],
                context_after: vec!["}".into()],
            }],
            truncated: false,
        }
        .into(),
        ProcessStartResult {
            process_id: "p1".into(),
            pid: 4242,
        }
        .into(),
        ProcessPollResult {
            running: false,
            exit_code: Some(0),
            signal: None,
            stdout: "ready".into(),
            stderr: String::new(),
            truncated: false,
        }
        .into(),
        ProcessStdinResult { bytes_written: 2 }.into(),
        ProcessKillResult { was_running: true }.into(),
        ProcessListResult {
            processes: vec![ProcessInfo {
                process_id: "p1".into(),
                pid: 4242,
                command: "npm run dev".into(),
                running: true,
                exit_code: None,
                pty: false,
            }],
        }
        .into(),
        UploadResult {
            path: "/w/img.png".into(),
            bytes_written: 2,
            created: false,
        }
        .into(),
        DownloadResult {
            path: "/w/img.png".into(),
            content_base64: "aGk=".into(),
            size: 2,
            mode: 0o600,
        }
        .into(),
    ]
}

#[test]
fn catalog_covers_every_tool_in_order() {
    let calls: Vec<_> = all_calls().iter().map(ToolCall::name).collect();
    let results: Vec<_> = all_results().iter().map(ToolResult::name).collect();
    assert_eq!(calls, TOOL_NAMES);
    assert_eq!(results, TOOL_NAMES);
    let defs: Vec<_> = tool_definitions().iter().map(|d| d.name).collect();
    assert_eq!(defs, TOOL_NAMES);
}

#[test]
fn tool_call_serde_tag_matches_method_name() {
    for call in all_calls() {
        let v = serde_json::to_value(&call).unwrap();
        assert_eq!(v["tool"], call.name());
        assert_eq!(v["params"], call.to_params());
        let back: ToolCall = serde_json::from_value(v).unwrap();
        assert_eq!(back, call);
    }
}

#[test]
fn every_call_round_trips_through_method_params_and_frames() {
    for (i, call) in all_calls().into_iter().enumerate() {
        let back = ToolCall::from_method_params(call.name(), call.to_params()).unwrap();
        assert_eq!(back, call);

        let call = Call::Tool(call);
        let frame = encode_frame(&call.to_request(i as i64)).unwrap();
        let mut dec = FrameDecoder::new();
        dec.push(&frame);
        let msg: RpcMessage = dec.next_message().unwrap().unwrap();
        let RpcMessage::Request(req) = msg else {
            panic!("expected request")
        };
        assert_eq!(req.id, RequestId::Number(i as i64));
        assert_eq!(Call::from_request(&req).unwrap(), call);
    }
}

#[test]
fn every_result_round_trips() {
    for result in all_results() {
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(serde_json::from_value::<ToolResult>(v).unwrap(), result);

        let back = ToolResult::from_method_value(result.name(), result.to_value()).unwrap();
        assert_eq!(back, result);

        let resp = RpcResponse::from_outcome("r".into(), Ok(Reply::Tool(result.clone())));
        let wire = serde_json::to_string(&resp).unwrap();
        let resp: RpcResponse = serde_json::from_str(&wire).unwrap();
        let reply = Reply::from_method_value(result.name(), resp.into_result().unwrap()).unwrap();
        assert_eq!(reply, Reply::Tool(result));
    }
}

#[test]
fn typed_result_extraction() {
    let r: ToolResult = ProcessKillResult { was_running: true }.into();
    assert!(ProcessKillResult::try_from(r.clone()).unwrap().was_running);
    let err = BashResult::try_from(r).unwrap_err();
    assert_eq!(err.code(), codes::INTERNAL_ERROR);
}

#[test]
fn request_wire_shape_is_json_rpc() {
    let req = Call::Tool(ToolCall::FileRead(FileReadParams {
        path: "a".into(),
        offset: None,
        limit: None,
    }))
    .to_request("abc");
    assert_eq!(
        serde_json::to_value(&req).unwrap(),
        json!({"jsonrpc": "2.0", "id": "abc", "method": "file_read", "params": {"path": "a"}})
    );
}

#[test]
fn response_wire_shapes() {
    let ok = RpcResponse::success(1.into(), json!({"x": 1}));
    assert_eq!(
        serde_json::to_value(&ok).unwrap(),
        json!({"jsonrpc": "2.0", "id": 1, "result": {"x": 1}})
    );
    let err = RpcResponse::failure(None, &ProtocolError::Parse("bad".into()));
    assert_eq!(
        serde_json::to_value(&err).unwrap(),
        json!({"jsonrpc": "2.0", "id": null, "error": {
            "code": -32700, "message": "parse error: bad", "data": {"detail": "bad"}
        }})
    );
    let back: RpcResponse = serde_json::from_value(serde_json::to_value(&err).unwrap()).unwrap();
    assert_eq!(back, err);
    assert_eq!(
        back.into_result().unwrap_err(),
        ProtocolError::Parse("bad".into())
    );
}

#[test]
fn defaults_apply_when_fields_are_omitted() {
    let call = ToolCall::from_method_params("list_dir", Value::Null).unwrap();
    assert_eq!(
        call,
        ToolCall::ListDir(ListDirParams {
            path: None,
            depth: None,
            respect_gitignore: true
        })
    );
    let call = ToolCall::from_method_params("process_kill", json!({"process_id": "p"})).unwrap();
    assert!(matches!(
        call,
        ToolCall::ProcessKill(ProcessKillParams {
            signal: KillSignal::Term,
            ..
        })
    ));
}

#[test]
fn unknown_fields_and_methods_are_rejected() {
    let err =
        ToolCall::from_method_params("bash", json!({"command": "ls", "shell": "zsh"})).unwrap_err();
    assert_eq!(err.code(), codes::INVALID_PARAMS);
    assert!(err.to_string().contains("shell"), "{err}");

    let err = ToolCall::from_method_params("bash", json!({})).unwrap_err();
    assert_eq!(err.code(), codes::INVALID_PARAMS);

    let err = ToolCall::from_method_params("bash", json!(["ls"])).unwrap_err();
    assert_eq!(err.code(), codes::INVALID_PARAMS);

    let err = Call::from_method_params("rm_rf", Value::Null).unwrap_err();
    assert_eq!(err, ProtocolError::MethodNotFound("rm_rf".into()));
    assert_eq!(err.code(), codes::METHOD_NOT_FOUND);
}

#[test]
fn hello_and_ping() {
    let hello = Call::from_method_params("hello", Value::Null).unwrap();
    assert_eq!(hello, Call::Hello(HelloParams::default()));
    assert_eq!(
        Call::from_method_params("ping", json!({})).unwrap(),
        Call::Ping
    );
    assert!(Call::from_method_params("ping", json!({"x": 1})).is_err());

    let reply = Reply::Hello(HelloResult {
        crabd_version: "0.1.0".into(),
        protocol_version: PROTOCOL_VERSION.into(),
        tools: TOOL_NAMES.iter().map(|s| s.to_string()).collect(),
        default_cwd: "/workspace".into(),
    });
    assert_eq!(
        Reply::from_method_value("hello", reply.to_value()).unwrap(),
        reply
    );
    assert_eq!(
        Reply::from_method_value("ping", json!({})).unwrap(),
        Reply::Ping(PingResult {})
    );
}

#[test]
fn message_classification() {
    let req = br#"{"jsonrpc":"2.0","id":"a","method":"ping"}"#;
    assert!(matches!(
        RpcMessage::from_slice(req).unwrap(),
        RpcMessage::Request(_)
    ));
    let note = br#"{"jsonrpc":"2.0","method":"stream_event","params":{"event":"process_exited","process_id":"p","exit_code":0,"signal":null}}"#;
    let RpcMessage::Notification(n) = RpcMessage::from_slice(note).unwrap() else {
        panic!()
    };
    assert_eq!(
        StreamEvent::from_notification(&n).unwrap(),
        StreamEvent::ProcessExited {
            process_id: "p".into(),
            exit_code: Some(0),
            signal: None
        }
    );
    let resp = br#"{"jsonrpc":"2.0","id":3,"result":{}}"#;
    assert!(matches!(
        RpcMessage::from_slice(resp).unwrap(),
        RpcMessage::Response(_)
    ));

    assert_eq!(
        RpcMessage::from_slice(b"{nope").unwrap_err().code(),
        codes::PARSE_ERROR
    );
    for bad in [
        &br#"[1,2]"#[..],
        br#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
        br#"{"jsonrpc":"2.0","id":1}"#,
        br#"{"jsonrpc":"2.0","id":{"x":1},"method":"ping"}"#,
    ] {
        assert_eq!(
            RpcMessage::from_slice(bad).unwrap_err().code(),
            codes::INVALID_REQUEST,
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
    assert_eq!(
        RpcMessage::sniff_id(br#"{"id":5,"method":42}"#),
        Some(RequestId::Number(5))
    );
}

#[test]
fn stream_event_round_trip() {
    let ev = StreamEvent::Output {
        request_id: Some(RequestId::random()),
        process_id: None,
        stream: OutputStream::Stderr,
        data: "warn".into(),
    };
    let n = ev.to_notification();
    let wire = serde_json::to_vec(&RpcMessage::Notification(n)).unwrap();
    let RpcMessage::Notification(n) = RpcMessage::from_slice(&wire).unwrap() else {
        panic!()
    };
    assert_eq!(StreamEvent::from_notification(&n).unwrap(), ev);
}
