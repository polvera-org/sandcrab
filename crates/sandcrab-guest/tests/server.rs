//! End-to-end protocol tests: an in-process `crabd` server on a temp Unix socket (and TCP),
//! plus a few checks against the built `crabd` binary.

use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use sandcrab_guest::{ListenAddr, Server, ServerConfig, ToolContext, codec};
use sandcrab_protocol::{
    MAX_FRAME_LEN, PROTOCOL_VERSION, RpcResponse, TOOL_NAMES, codes, encode_frame,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{io::AsyncWriteExt, net::UnixStream, sync::oneshot, task::JoinHandle};

struct TestServer {
    dir: TempDir,
    sock: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    handle: JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("crabd.sock");
        let (stop, handle) = spawn_server(ListenAddr::Unix(sock.clone()), dir.path()).await;
        Self {
            dir,
            sock,
            stop: Some(stop),
            handle,
        }
    }

    async fn connect(&self) -> UnixStream {
        UnixStream::connect(&self.sock).await.unwrap()
    }

    async fn shutdown(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.handle.await.unwrap().unwrap();
        assert!(!self.sock.exists(), "socket file removed on shutdown");
    }
}

async fn spawn_server(
    listen: ListenAddr,
    cwd: &Path,
) -> (oneshot::Sender<()>, JoinHandle<std::io::Result<()>>) {
    let (tx, rx) = oneshot::channel::<()>();
    let server = Server::bind(ServerConfig::new(listen), Arc::new(ToolContext::new(cwd)))
        .await
        .unwrap();
    let handle = tokio::spawn(server.run(async {
        let _ = rx.await;
    }));
    (tx, handle)
}

async fn send<S: AsyncWriteExt + Unpin>(s: &mut S, msg: &Value) {
    s.write_all(&encode_frame(msg).unwrap()).await.unwrap();
}

async fn send_raw<S: AsyncWriteExt + Unpin>(s: &mut S, payload: &[u8]) {
    let len = u32::try_from(payload.len()).unwrap();
    s.write_all(&len.to_be_bytes()).await.unwrap();
    s.write_all(payload).await.unwrap();
}

async fn recv<S: tokio::io::AsyncRead + Unpin>(s: &mut S) -> Value {
    let payload = tokio::time::timeout(Duration::from_secs(5), codec::read_frame(s))
        .await
        .expect("response within 5s")
        .unwrap()
        .expect("a frame, not EOF");
    // Every response must be a valid typed JSON-RPC response.
    let _: RpcResponse = serde_json::from_slice(&payload).unwrap();
    serde_json::from_slice(&payload).unwrap()
}

fn req(id: impl Into<Value>, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.into(), "method": method, "params": params })
}

fn error_code(resp: &Value) -> i64 {
    resp["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("expected error, got {resp}"))
}

#[tokio::test]
async fn hello_reports_version_tools_and_cwd() {
    let srv = TestServer::start().await;
    let mut c = srv.connect().await;
    send(
        &mut c,
        &req(
            1,
            "hello",
            json!({ "protocol_version": PROTOCOL_VERSION, "client": "test" }),
        ),
    )
    .await;
    let r = recv(&mut c).await;
    assert_eq!(r["id"], 1);
    let res = &r["result"];
    assert_eq!(res["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(res["crabd_version"], sandcrab_guest::CRABD_VERSION);
    let tools: Vec<&str> = res["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert_eq!(tools, TOOL_NAMES);
    let cwd = srv.dir.path().display().to_string();
    assert_eq!(res["default_cwd"], cwd.as_str());
    srv.shutdown().await;
}

#[tokio::test]
async fn ping_with_and_without_params() {
    let srv = TestServer::start().await;
    let mut c = srv.connect().await;
    send(&mut c, &req("a", "ping", json!({}))).await;
    send(
        &mut c,
        &json!({ "jsonrpc": "2.0", "id": "b", "method": "ping" }),
    )
    .await;
    let mut got = HashMap::new();
    for _ in 0..2 {
        let r = recv(&mut c).await;
        got.insert(r["id"].as_str().unwrap().to_owned(), r["result"].clone());
    }
    assert_eq!(got["a"], json!({}));
    assert_eq!(got["b"], json!({}));
    srv.shutdown().await;
}

#[tokio::test]
async fn unknown_method_is_32601() {
    let srv = TestServer::start().await;
    let mut c = srv.connect().await;
    send(&mut c, &req(7, "rm_rf", json!({}))).await;
    let r = recv(&mut c).await;
    assert_eq!(r["id"], 7);
    assert_eq!(error_code(&r), codes::METHOD_NOT_FOUND);
    assert!(r["error"]["message"].as_str().unwrap().contains("rm_rf"));
    srv.shutdown().await;
}

#[tokio::test]
async fn invalid_params_is_32602_with_serde_message() {
    let srv = TestServer::start().await;
    let mut c = srv.connect().await;
    send(&mut c, &req(1, "bash", json!({}))).await;
    let r = recv(&mut c).await;
    assert_eq!(error_code(&r), codes::INVALID_PARAMS);
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing field `command`"),
        "{r}"
    );

    send(
        &mut c,
        &req(2, "file_read", json!({ "path": "x", "bogus": 1 })),
    )
    .await;
    let r = recv(&mut c).await;
    assert_eq!(error_code(&r), codes::INVALID_PARAMS);
    assert!(r["error"]["message"].as_str().unwrap().contains("bogus"));

    send(&mut c, &req(3, "ping", json!([1, 2]))).await;
    let r = recv(&mut c).await;
    assert_eq!(error_code(&r), codes::INVALID_PARAMS);
    srv.shutdown().await;
}

#[tokio::test]
async fn concurrent_requests_on_one_connection() {
    let srv = TestServer::start().await;
    let mut c = srv.connect().await;
    // Pipelined without waiting: two hellos and a (stubbed) tool call.
    send(&mut c, &req(1, "hello", json!({}))).await;
    send(&mut c, &req("two", "hello", json!({}))).await;
    send(&mut c, &req(3, "bash", json!({ "command": "true" }))).await;
    let mut by_id = HashMap::new();
    for _ in 0..3 {
        let r = recv(&mut c).await;
        by_id.insert(r["id"].to_string(), r);
    }
    assert!(by_id["1"]["result"]["tools"].is_array());
    assert!(by_id["\"two\""]["result"]["tools"].is_array());
    // Stubbed today (Unimplemented); a real result once the exec step lands.
    let bash = &by_id["3"];
    assert!(
        bash["result"].is_object() || error_code(bash) == codes::UNIMPLEMENTED,
        "{bash}"
    );
    srv.shutdown().await;
}

#[tokio::test]
async fn many_connections_in_parallel() {
    let srv = TestServer::start().await;
    let mut tasks = Vec::new();
    for i in 0..16 {
        let mut c = srv.connect().await;
        tasks.push(tokio::spawn(async move {
            send(&mut c, &req(i, "ping", json!({}))).await;
            recv(&mut c).await["id"].as_i64().unwrap()
        }));
    }
    for (i, t) in tasks.into_iter().enumerate() {
        assert_eq!(t.await.unwrap(), i64::try_from(i).unwrap());
    }
    srv.shutdown().await;
}

#[tokio::test]
async fn every_tool_is_routed() {
    let srv = TestServer::start().await;
    let mut c = srv.connect().await;
    let calls = [
        ("bash", json!({ "command": "true" })),
        ("file_read", json!({ "path": "a" })),
        ("file_write", json!({ "path": "a", "content": "" })),
        (
            "file_edit",
            json!({ "path": "a", "old_string": "x", "new_string": "y" }),
        ),
        ("file_delete", json!({ "path": "a" })),
        ("file_move", json!({ "from": "a", "to": "b" })),
        ("list_dir", json!({})),
        ("glob", json!({ "pattern": "*" })),
        ("grep", json!({ "pattern": "x" })),
        ("process_start", json!({ "command": "true" })),
        ("process_poll", json!({ "process_id": "p" })),
        ("process_stdin", json!({ "process_id": "p", "data": "" })),
        ("process_kill", json!({ "process_id": "p" })),
        ("process_list", json!({})),
        ("upload", json!({ "path": "a", "content_base64": "" })),
        ("download", json!({ "path": "a" })),
    ];
    assert_eq!(calls.len(), TOOL_NAMES.len());
    for (i, (method, params)) in calls.iter().enumerate() {
        send(&mut c, &req(i, method, params.clone())).await;
    }
    // Routing check only: whatever a tool answers (stub `Unimplemented`, a result, or a
    // tool error), it must not be a dispatch-level failure.
    for _ in 0..calls.len() {
        let r = recv(&mut c).await;
        if r.get("error").is_some() {
            let code = error_code(&r);
            assert!(
                ![
                    codes::METHOD_NOT_FOUND,
                    codes::INVALID_PARAMS,
                    codes::INTERNAL_ERROR
                ]
                .contains(&code),
                "{r}"
            );
        }
    }
    srv.shutdown().await;
}

#[tokio::test]
async fn malformed_json_is_32700_and_connection_survives() {
    let srv = TestServer::start().await;
    let mut c = srv.connect().await;
    send_raw(&mut c, b"{not json").await;
    let r = recv(&mut c).await;
    assert_eq!(error_code(&r), codes::PARSE_ERROR);
    assert!(r["id"].is_null());
    send(&mut c, &req(2, "ping", json!({}))).await;
    assert_eq!(recv(&mut c).await["id"], 2);
    srv.shutdown().await;
}

#[tokio::test]
async fn invalid_request_is_32600_with_sniffed_id() {
    let srv = TestServer::start().await;
    let mut c = srv.connect().await;
    send(
        &mut c,
        &json!({ "jsonrpc": "1.0", "id": 9, "method": "ping" }),
    )
    .await;
    let r = recv(&mut c).await;
    assert_eq!(error_code(&r), codes::INVALID_REQUEST);
    assert_eq!(r["id"], 9);

    send(&mut c, &json!([1, 2, 3])).await;
    let r = recv(&mut c).await;
    assert_eq!(error_code(&r), codes::INVALID_REQUEST);
    assert!(r["id"].is_null());
    srv.shutdown().await;
}

#[tokio::test]
async fn notifications_are_ignored() {
    let srv = TestServer::start().await;
    let mut c = srv.connect().await;
    send(&mut c, &json!({ "jsonrpc": "2.0", "method": "ping" })).await;
    send(&mut c, &req(1, "ping", json!({}))).await;
    assert_eq!(recv(&mut c).await["id"], 1);
    srv.shutdown().await;
}

#[tokio::test]
async fn oversize_frame_is_rejected_and_connection_closed() {
    let srv = TestServer::start().await;
    let mut c = srv.connect().await;
    let len = u32::try_from(MAX_FRAME_LEN + 1).unwrap();
    c.write_all(&len.to_be_bytes()).await.unwrap();
    let r = recv(&mut c).await;
    assert_eq!(error_code(&r), codes::TOO_LARGE);
    assert!(r["id"].is_null());
    let next = tokio::time::timeout(Duration::from_secs(5), codec::read_frame(&mut c))
        .await
        .unwrap();
    assert!(matches!(next, Ok(None)), "connection closed: {next:?}");
    srv.shutdown().await;
}

#[tokio::test]
async fn zero_length_frame_is_rejected_and_connection_closed() {
    let srv = TestServer::start().await;
    let mut c = srv.connect().await;
    c.write_all(&0u32.to_be_bytes()).await.unwrap();
    let r = recv(&mut c).await;
    assert_eq!(error_code(&r), codes::INVALID_REQUEST);
    assert!(matches!(codec::read_frame(&mut c).await, Ok(None)));
    srv.shutdown().await;
}

#[tokio::test]
async fn socket_is_0660_and_stale_socket_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("crabd.sock");
    // A stale socket: bound then dropped, file left behind.
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    assert!(sock.exists());
    // A child forked concurrently by another test may still hold the listener fd until its
    // exec, keeping the socket briefly live (bind would then rightly say AddrInUse).
    for _ in 0..200 {
        if std::os::unix::net::UnixStream::connect(&sock).is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (stop, handle) = spawn_server(ListenAddr::Unix(sock.clone()), dir.path()).await;
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o660);

    // A live socket is not stolen.
    let err = Server::bind(
        ServerConfig::new(ListenAddr::Unix(sock.clone())),
        Arc::new(ToolContext::new("/")),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
    assert!(!sock.exists());
}

#[tokio::test]
async fn refuses_to_replace_non_socket_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-a-socket");
    std::fs::write(&path, "keep me").unwrap();
    let err = Server::bind(
        ServerConfig::new(ListenAddr::Unix(path.clone())),
        Arc::new(ToolContext::new("/")),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn vsock_requires_linux() {
    let err = Server::bind(
        ServerConfig::new(ListenAddr::Vsock(1024)),
        Arc::new(ToolContext::new("/")),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
}

/// Binding needs a vsock transport (a VM guest, or vsock_loopback); either it binds or the
/// error names the port.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn vsock_binds_or_reports_transport() {
    match Server::bind(
        ServerConfig::new(ListenAddr::Vsock(50_000 + std::process::id() % 10_000)),
        Arc::new(ToolContext::new("/")),
    )
    .await
    {
        Ok(server) => assert!(server.endpoint().starts_with("vsock:")),
        Err(e) => assert!(e.to_string().contains("vsock port"), "{e}"),
    }
}

#[tokio::test]
async fn tcp_transport() {
    let server = Server::bind(
        ServerConfig::new(ListenAddr::Tcp("127.0.0.1:0".into())),
        Arc::new(ToolContext::new("/")),
    )
    .await
    .unwrap();
    let addr = server.tcp_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(server.run(async {
        let _ = rx.await;
    }));
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    send(&mut c, &req(1, "ping", json!({}))).await;
    assert_eq!(recv(&mut c).await["result"], json!({}));
    tx.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn shutdown_closes_idle_connections() {
    let srv = TestServer::start().await;
    let mut c = srv.connect().await;
    send(&mut c, &req(1, "ping", json!({}))).await;
    recv(&mut c).await;
    let sock = srv.sock.clone();
    srv.shutdown().await;
    assert!(matches!(codec::read_frame(&mut c).await, Ok(None)));
    assert!(UnixStream::connect(&sock).await.is_err());
}

// ---- the built binary -------------------------------------------------------------------

const CRABD: &str = env!("CARGO_BIN_EXE_crabd");

#[tokio::test]
async fn binary_serves_and_exits_cleanly_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("crabd.sock");
    let mut child = tokio::process::Command::new(CRABD)
        .args(["serve", "--unix"])
        .arg(&sock)
        .arg("--default-cwd")
        .arg(dir.path())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut c = None;
    for _ in 0..250 {
        if let Ok(s) = UnixStream::connect(&sock).await {
            c = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut c = c.expect("crabd socket came up");
    send(&mut c, &req(1, "hello", json!({}))).await;
    let r = recv(&mut c).await;
    assert_eq!(r["result"]["protocol_version"], PROTOCOL_VERSION);

    let pid = child.id().unwrap().to_string();
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "exit status {status:?}");
    assert!(!sock.exists());
}

/// Every tool process of `crabd serve` runs with `no_new_privs`, as under Docker's
/// `no-new-privileges`: the microVM backends have no other place that sets it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn binary_tool_processes_have_no_new_privs() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("crabd.sock");
    let mut child = tokio::process::Command::new(CRABD)
        .args(["serve", "--unix"])
        .arg(&sock)
        .arg("--default-cwd")
        .arg(dir.path())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut c = None;
    for _ in 0..250 {
        if let Ok(s) = UnixStream::connect(&sock).await {
            c = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut c = c.expect("crabd socket came up");
    let cmd = "grep '^NoNewPrivs:' /proc/self/status";
    send(&mut c, &req(1, "bash", json!({ "command": cmd }))).await;
    let r = recv(&mut c).await;
    assert_eq!(r["result"]["stdout"], "NoNewPrivs:\t1\n", "{r}");
    child.kill().await.unwrap();
}

#[test]
fn binary_vsock_flag() {
    let port = (60_000 + std::process::id() % 5_000).to_string();
    let mut child = std::process::Command::new(CRABD)
        .args(["serve", "--vsock", &port])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    if child.try_wait().unwrap().is_none() {
        // Bound and serving: a vsock transport is available (Linux only).
        child.kill().unwrap();
        child.wait().unwrap();
        return;
    }
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let want = if cfg!(target_os = "linux") {
        "vsock port"
    } else {
        "requires Linux"
    };
    assert!(stderr.contains(want), "{stderr}");
}

#[test]
fn binary_requires_exactly_one_transport() {
    let none = std::process::Command::new(CRABD)
        .arg("serve")
        .output()
        .unwrap();
    assert_eq!(none.status.code(), Some(2));
    let two = std::process::Command::new(CRABD)
        .args(["serve", "--unix", "/tmp/x.sock", "--tcp", "127.0.0.1:0"])
        .output()
        .unwrap();
    assert_eq!(two.status.code(), Some(2));
}
