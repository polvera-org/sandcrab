//! `process.open` / `process.cancel` against a real `crabd serve` and real `crabd supervise`
//! processes, on a Unix socket. Linux only (the tree kill reads `/proc`).
//!
//! By default each test starts an in-process server whose supervisor is the `crabd` binary of
//! this build. With `SANDCRAB_TEST_CRABD_SOCKET=<path>` the tests talk to an external
//! `crabd serve` instead, e.g. one in a hardened Docker container. Everything the tests check
//! inside the sandbox (files, liveness of a pid) goes through `crabd` itself, so the same
//! tests run in both places.
#![cfg(target_os = "linux")]

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use sandcrab_guest::{ListenAddr, Server, ServerConfig, ToolContext, codec};
use sandcrab_protocol::{
    ExecLimits, OutputCap, Overflow, ProcessCancelResult, ProcessExit, ProcessOpenAck,
    ProcessOpenParams, RecordChannel, RecordDecoder, RecordKind, ResponsePayload, RpcRequest,
    RpcResponse, StderrMode, StdinMode, codes, encode_record,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::oneshot,
};

const SOCKET_ENV: &str = "SANDCRAB_TEST_CRABD_SOCKET";
const LONG: Duration = Duration::from_secs(30);

struct Target {
    sock: PathBuf,
    /// In-process server: its stop signal and the temp dir that holds its socket.
    _local: Option<(oneshot::Sender<()>, tempfile::TempDir)>,
}

async fn target() -> Target {
    if let Some(sock) = std::env::var_os(SOCKET_ENV) {
        return Target {
            sock: sock.into(),
            _local: None,
        };
    }
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("crabd.sock");
    let mut config = ServerConfig::new(ListenAddr::Unix(sock.clone()));
    config.shutdown_grace = Duration::from_millis(500);
    config.supervisor.exe = env!("CARGO_BIN_EXE_crabd").into();
    let server = Server::bind(config, Arc::new(ToolContext::new("/tmp")))
        .await
        .unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(server.run(async {
        let _ = rx.await;
    }));
    Target {
        sock,
        _local: Some((tx, dir)),
    }
}

fn uuid() -> String {
    // Unique enough for tests and a valid UUID shape.
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let r = u128::from(std::process::id()) << 64 ^ n ^ u128::from(rand_u64());
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (r >> 96) as u32,
        (r >> 80) as u16,
        (r >> 64) as u16 & 0xfff,
        (r >> 48) as u16 & 0xfff,
        r as u64 & 0xffff_ffff_ffff
    )
}

fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(0);
    h.finish()
}

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}

fn params(a: &[&str]) -> ProcessOpenParams {
    let mut p = ProcessOpenParams::new(uuid(), argv(a));
    p.env.insert("PATH".into(), "/usr/bin:/bin".into());
    p
}

async fn recv(s: &mut UnixStream) -> RpcResponse {
    let payload = tokio::time::timeout(LONG, codec::read_frame(s))
        .await
        .expect("frame in time")
        .unwrap()
        .expect("a frame, not EOF");
    serde_json::from_slice(&payload).unwrap()
}

fn error_of(r: &RpcResponse) -> (i64, Value) {
    match &r.payload {
        ResponsePayload::Error(e) => (e.code, e.data.clone().unwrap_or(Value::Null)),
        ResponsePayload::Result(v) => panic!("expected an error, got {v}"),
    }
}

fn result_of(r: &RpcResponse) -> Value {
    match &r.payload {
        ResponsePayload::Result(v) => v.clone(),
        ResponsePayload::Error(e) => panic!("expected a result, got {e:?}"),
    }
}

/// One request on a fresh tool connection.
async fn call(t: &Target, method: &str, params: Value) -> RpcResponse {
    let mut s = UnixStream::connect(&t.sock).await.unwrap();
    codec::write_frame(&mut s, &RpcRequest::new(1, method, Some(params)))
        .await
        .unwrap();
    recv(&mut s).await
}

async fn cancel(t: &Target, id: &str, grace_ms: u64) -> ProcessCancelResult {
    let r = call(
        t,
        "process.cancel",
        json!({"process_id": id, "grace_ms": grace_ms}),
    )
    .await;
    serde_json::from_value(result_of(&r)).unwrap()
}

/// Send the open frame; return the stream and the response.
async fn open(t: &Target, p: &ProcessOpenParams) -> (UnixStream, RpcResponse) {
    let mut s = UnixStream::connect(&t.sock).await.unwrap();
    codec::write_frame(&mut s, &p.to_request(7)).await.unwrap();
    let r = recv(&mut s).await;
    (s, r)
}

#[derive(Debug, Default)]
struct Output {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: Option<ProcessExit>,
}

/// Read records until `EXIT`, then expect the connection to close.
async fn collect<R: tokio::io::AsyncRead + Unpin>(rd: &mut R) -> Output {
    let mut dec = RecordDecoder::new(RecordChannel::ProcessOutput);
    let mut out = Output::default();
    let mut buf = vec![0u8; 128 * 1024];
    tokio::time::timeout(LONG, async {
        loop {
            while let Some(r) = dec.next_record().unwrap() {
                assert!(out.exit.is_none(), "a record after EXIT");
                match r.kind {
                    RecordKind::Stdout => out.stdout.extend(r.payload),
                    RecordKind::Stderr => out.stderr.extend(r.payload),
                    RecordKind::Exit => {
                        out.exit = Some(ProcessExit::from_payload(&r.payload).unwrap())
                    }
                    k => panic!("unexpected {k}"),
                }
            }
            let n = rd.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                dec.finish().unwrap();
                return;
            }
            dec.push(&buf[..n]);
        }
    })
    .await
    .expect("EXIT and close in time");
    out
}

/// Run to the end and return the output (stdin is whatever `p` says, never written).
async fn run(t: &Target, p: &ProcessOpenParams) -> Output {
    let (mut s, r) = open(t, p).await;
    let ack: ProcessOpenAck = ProcessOpenAck::from_value(result_of(&r)).unwrap();
    assert_eq!(ack.process_id, p.process_id);
    collect(&mut s).await
}

/// Whether `pid` exists inside the sandbox, asked through `crabd` itself.
async fn alive(t: &Target, pid: u32) -> bool {
    let out = run(t, &params(&["/bin/sh", "-c", &format!("kill -0 {pid}")])).await;
    out.exit.unwrap().code == Some(0)
}

async fn wait_gone(t: &Target, pid: u32, within: Duration) {
    let deadline = Instant::now() + within;
    while alive(t, pid).await {
        assert!(Instant::now() < deadline, "pid {pid} still alive");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A leader that ignores TERM and a grandchild in a new session that ignores TERM too.
/// Prints the grandchild's pid as the first line.
const STUBBORN: &str = r#"trap "" TERM
setsid /bin/sh -c 'trap "" TERM; echo $$; exec sleep 1000' &
while :; do sleep 0.05; done"#;

/// Read stdout records until a full line arrived; return it.
async fn first_line(s: &mut UnixStream) -> String {
    let mut dec = RecordDecoder::new(RecordChannel::ProcessOutput);
    let mut line = Vec::new();
    let mut buf = [0u8; 4096];
    tokio::time::timeout(LONG, async {
        loop {
            let n = s.read(&mut buf).await.unwrap();
            assert!(n > 0, "closed before a line");
            dec.push(&buf[..n]);
            while let Some(r) = dec.next_record().unwrap() {
                assert_eq!(r.kind, RecordKind::Stdout);
                line.extend(r.payload);
            }
            if let Some(i) = line.iter().position(|&b| b == b'\n') {
                return String::from_utf8(line[..i].to_vec()).unwrap();
            }
        }
    })
    .await
    .expect("a line in time")
}

#[tokio::test]
async fn hello_reports_0_3_features_and_refuses_0_2() {
    let t = target().await;
    let r = call(&t, "hello", json!({"protocol_version": "0.3"})).await;
    let v = result_of(&r);
    assert_eq!(v["protocol_version"], "0.3");
    let features: Vec<String> = serde_json::from_value(v["features"].clone()).unwrap();
    assert!(
        features.contains(&"process.open".to_owned()),
        "{features:?}"
    );
    assert!(
        features.contains(&"tree_kill:subreaper".to_owned()),
        "{features:?}"
    );
    assert!(
        !v["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("process.open"))
    );

    let r = call(&t, "hello", json!({"protocol_version": "0.2"})).await;
    let (code, data) = error_of(&r);
    assert_eq!(code, codes::UNSUPPORTED_VERSION);
    assert_eq!(data, json!({"crabd": "0.3", "host": "0.2"}));
    // Without a version it is still answered.
    result_of(&call(&t, "hello", json!({})).await);
}

#[tokio::test]
async fn exact_env_and_argv_without_a_shell() {
    let t = target().await;
    let mut p = params(&["/usr/bin/env"]);
    p.env = BTreeMap::from([
        ("A".into(), "1".into()),
        ("B".into(), "x y; $HOME *".into()),
    ]);
    let out = run(&t, &p).await;
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "A=1\nB=x y; $HOME *\n"
    );
    let exit = out.exit.unwrap();
    assert_eq!((exit.code, exit.signal), (Some(0), None));
    assert!(exit.tree_clean && !exit.cancelled && !exit.timed_out);

    // Each argv element arrives as is: no splitting, no expansion.
    let out = run(
        &t,
        &params(&["/usr/bin/printf", "%s|", "a b", "$HOME", "*", "'q'", ""]),
    )
    .await;
    assert_eq!(out.stdout, b"a b|$HOME|*|'q'||");

    // An empty env: nothing at all, not even PATH.
    let mut p = params(&["/usr/bin/env"]);
    p.env.clear();
    assert_eq!(run(&t, &p).await.stdout, b"");

    // cwd is honoured; stderr (default `tail`) arrives in the exit only.
    let mut p = params(&["/bin/sh", "-c", "pwd; echo oops >&2; exit 3"]);
    p.cwd = Some("/usr".into());
    let out = run(&t, &p).await;
    assert_eq!(out.stdout, b"/usr\n");
    assert!(out.stderr.is_empty());
    let exit = out.exit.unwrap();
    assert_eq!(exit.code, Some(3));
    assert_eq!(exit.stderr_tail, "oops\n");
}

#[tokio::test]
async fn one_mib_of_mixed_bytes_arrives_identical_and_exit_is_last() {
    let t = target().await;
    let mut data = Vec::with_capacity(1 << 20);
    let mut i: u32 = 0;
    while data.len() < 1 << 20 {
        match i % 4 {
            0 => data.extend_from_slice("héllo wörld ✓ 漢字\r\n".as_bytes()),
            1 => data.extend_from_slice(b"line\r\n\r\n"),
            2 => data.extend((0..=255u8).map(|b| b.wrapping_mul(31).wrapping_add(i as u8))),
            _ => data.extend_from_slice("🦀\n".as_bytes()),
        }
        i += 1;
    }
    data.truncate(1 << 20);

    let mut p = params(&["/bin/cat"]);
    p.stdin = StdinMode::Pipe;
    let (s, r) = open(&t, &p).await;
    result_of(&r);
    let (mut rd, mut wr) = s.into_split();
    let input = data.clone();
    let writer = tokio::spawn(async move {
        for chunk in input.chunks(50_000) {
            wr.write_all(&encode_record(RecordKind::Stdin, chunk).unwrap())
                .await
                .unwrap();
        }
        wr.write_all(&encode_record(RecordKind::StdinEof, b"").unwrap())
            .await
            .unwrap();
        wr
    });
    let out = collect(&mut rd).await;
    let _wr = writer.await.unwrap();
    assert_eq!(out.stdout.len(), data.len());
    assert!(out.stdout == data, "bytes differ");
    let exit = out.exit.unwrap();
    assert_eq!(exit.code, Some(0));
    assert!(exit.tree_clean);
}

#[tokio::test]
async fn stdin_eof_versus_kept_open() {
    let t = target().await;
    // `null`: cat sees EOF at once.
    let out = run(&t, &params(&["/bin/cat"])).await;
    assert_eq!((out.stdout.len(), out.exit.unwrap().code), (0, Some(0)));

    // `pipe`, kept open: the program keeps running after it echoed the bytes.
    let mut p = params(&["/bin/cat"]);
    p.stdin = StdinMode::Pipe;
    let (mut s, r) = open(&t, &p).await;
    result_of(&r);
    s.write_all(&encode_record(RecordKind::Stdin, b"one\n").unwrap())
        .await
        .unwrap();
    assert_eq!(first_line(&mut s).await, "one");
    let mut b = [0u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(300), s.read(&mut b))
            .await
            .is_err(),
        "nothing more while stdin is open"
    );
    // STDIN_EOF ends it.
    s.write_all(&encode_record(RecordKind::StdinEof, b"").unwrap())
        .await
        .unwrap();
    let out = collect(&mut s).await;
    assert!(out.stdout.is_empty());
    assert_eq!(out.exit.unwrap().code, Some(0));
}

#[tokio::test]
async fn cancel_kills_a_term_ignoring_setsid_grandchild() {
    let t = target().await;
    let p = params(&["/bin/sh", "-c", STUBBORN]);
    let (mut s, r) = open(&t, &p).await;
    let ack = ProcessOpenAck::from_value(result_of(&r)).unwrap();
    let gc: u32 = first_line(&mut s).await.parse().unwrap();
    assert!(alive(&t, gc).await);

    let started = Instant::now();
    let res = cancel(&t, &p.process_id, 200).await;
    assert!(res.was_running && res.tree_clean, "{res:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    // Gone when the cancel answered.
    assert!(!alive(&t, gc).await, "grandchild {gc} survived");
    assert!(!alive(&t, ack.pid).await, "leader survived");

    let exit = collect(&mut s).await.exit.unwrap();
    assert!(
        exit.cancelled && exit.tree_clean && !exit.timed_out,
        "{exit:?}"
    );
    assert_eq!(exit.signal, Some(9));

    // Twice is fine; the finished id is not running.
    let again = cancel(&t, &p.process_id, 0).await;
    assert!(!again.was_running);
}

#[tokio::test]
async fn closing_the_connection_kills_the_tree() {
    let t = target().await;
    let mut p = params(&["/bin/sh", "-c", STUBBORN]);
    p.kill_grace_ms = Some(100);
    let (mut s, r) = open(&t, &p).await;
    result_of(&r);
    let gc: u32 = first_line(&mut s).await.parse().unwrap();
    drop(s);
    wait_gone(&t, gc, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn a_leader_that_leaves_a_grandchild_on_its_pipes_ends_within_the_drain() {
    let t = target().await;
    let started = Instant::now();
    let out = run(
        &t,
        &params(&["/bin/sh", "-c", "setsid sleep 1000 & echo left; exit 4"]),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(out.stdout, b"left\n");
    let exit = out.exit.unwrap();
    assert_eq!(exit.code, Some(4));
    assert!(exit.tree_clean);
}

#[tokio::test]
async fn spawn_failures_are_spawn_failed() {
    let t = target().await;
    let check = |r: RpcResponse, errno: &str, what: &str| {
        let (code, data) = error_of(&r);
        assert_eq!(code, codes::SPAWN_FAILED, "{data}");
        assert_eq!(data, json!({"errno": errno, "what": what}));
    };
    let (mut s, r) = open(&t, &params(&["/nonexistent/prog"])).await;
    check(r, "ENOENT", "program");
    let mut b = [0u8; 1];
    assert_eq!(s.read(&mut b).await.unwrap(), 0, "closed after the error");

    // Not on PATH (no shell, so no "command not found" exit 127).
    check(
        open(&t, &params(&["no-such-program-xyz"])).await.1,
        "ENOENT",
        "program",
    );

    // A file without the exec bit.
    let path = format!("/tmp/noexec-{}", uuid());
    let w = call(
        &t,
        "file_write",
        json!({"path": path, "content": "#!/bin/sh\n"}),
    )
    .await;
    result_of(&w);
    check(open(&t, &params(&[&path])).await.1, "EACCES", "program");
    call(&t, "file_delete", json!({"path": path})).await;

    let mut p = params(&["/bin/true"]);
    p.cwd = Some("/nonexistent-dir".into());
    check(open(&t, &p).await.1, "ENOENT", "cwd");
}

#[tokio::test]
async fn bad_params_and_misplaced_opens() {
    let t = target().await;
    let mut p = params(&["/bin/true"]);
    p.process_id = "not-a-uuid".into();
    assert_eq!(error_of(&open(&t, &p).await.1).0, codes::INVALID_PARAMS);

    // `process.open` after another frame is not a process connection.
    let mut s = UnixStream::connect(&t.sock).await.unwrap();
    codec::write_frame(&mut s, &RpcRequest::new(1, "ping", Some(json!({}))))
        .await
        .unwrap();
    result_of(&recv(&mut s).await);
    codec::write_frame(&mut s, &params(&["/bin/true"]).to_request(2))
        .await
        .unwrap();
    assert_eq!(error_of(&recv(&mut s).await).0, codes::INVALID_REQUEST);
}

#[tokio::test]
async fn a_cancel_around_the_open_is_never_lost() {
    let t = target().await;
    // Before the open: remembered, the open is CANCELLED.
    let p = params(&["/bin/sleep", "1000"]);
    let res = cancel(&t, &p.process_id, 0).await;
    assert!(!res.was_running);
    let (code, _) = error_of(&open(&t, &p).await.1);
    assert_eq!(code, codes::CANCELLED);

    // Right after the open frame was sent: either CANCELLED or it ran and was cancelled.
    for _ in 0..10 {
        let p = params(&["/bin/sleep", "1000"]);
        let mut s = UnixStream::connect(&t.sock).await.unwrap();
        codec::write_frame(&mut s, &p.to_request(1)).await.unwrap();
        let res = cancel(&t, &p.process_id, 0).await;
        let r = recv(&mut s).await;
        match &r.payload {
            ResponsePayload::Error(e) => assert_eq!(e.code, codes::CANCELLED),
            ResponsePayload::Result(_) => {
                let exit = collect(&mut s).await.exit.unwrap();
                assert!(exit.cancelled && exit.tree_clean, "{exit:?} {res:?}");
            }
        }
    }
}

#[tokio::test]
async fn fifty_concurrent_processes() {
    let t = Arc::new(target().await);
    let mut tasks = Vec::new();
    for i in 0..50 {
        let t = Arc::clone(&t);
        tasks.push(tokio::spawn(async move {
            let out = run(
                &t,
                &params(&["/bin/sh", "-c", &format!("sleep 0.2; printf {i}")]),
            )
            .await;
            assert_eq!(out.stdout, i.to_string().as_bytes());
            assert_eq!(out.exit.unwrap().code, Some(0));
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test]
async fn the_force_loop_beats_a_forking_tree() {
    let t = target().await;
    // Forks new TERM-ignoring sessions as fast as it can, up to a bound.
    let script = r#"trap "" TERM
echo go
i=0
while [ $i -lt 400 ]; do setsid /bin/sh -c 'trap "" TERM; sleep 4321.5' & i=$((i+1)); done
while :; do sleep 0.05; done"#;
    let p = params(&["/bin/sh", "-c", script]);
    let (mut s, r) = open(&t, &p).await;
    result_of(&r);
    assert_eq!(first_line(&mut s).await, "go");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let res = cancel(&t, &p.process_id, 0).await;
    assert!(res.was_running && res.tree_clean, "{res:?}");
    let exit = collect(&mut s).await.exit.unwrap();
    assert!(exit.tree_clean && exit.cancelled);
    // Nothing of it is left (the duration is unique to this test; `ps` may be missing in an
    // image, then this check is empty).
    let count = "ps -eo args 2>/dev/null | grep -c '[s]leep 4321.5' || true";
    let out = run(&t, &params(&["/bin/sh", "-c", count])).await;
    let left = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    assert!(left.is_empty() || left == "0", "left: {left}");
}

#[tokio::test]
async fn fs_identity_matches_stat() {
    let t = target().await;
    let r = call(&t, "fs.identity", json!({"paths": ["/", "/tmp"]})).await;
    let v = result_of(&r);
    let ids = v.as_array().unwrap();
    assert_eq!(ids.len(), 2);
    if std::env::var_os(SOCKET_ENV).is_none() {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::metadata("/tmp").unwrap();
        assert_eq!(ids[1], json!({"dev": md.dev(), "ino": md.ino()}));
    }
    let r = call(&t, "fs.identity", json!({"paths": ["/nonexistent-x"]})).await;
    assert_eq!(error_of(&r).0, codes::NOT_FOUND);
    let r = call(&t, "fs.identity", json!({"paths": ["relative"]})).await;
    assert_eq!(error_of(&r).0, codes::INVALID_PARAMS);
}

/// Echo through stdin and stdout, then a closed connection stops a stubborn tree, on
/// connections made by `connect` (any transport).
async fn handover_checks<S, F, Fut>(unix: &Target, connect: F, label: &str)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = S>,
{
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let mut p = params(&["/bin/cat"]);
    p.stdin = StdinMode::Pipe;
    let mut s = connect().await;
    codec::write_frame(&mut s, &p.to_request(1)).await.unwrap();
    let payload = codec::read_frame(&mut s).await.unwrap().unwrap();
    result_of(&serde_json::from_slice(&payload).unwrap());
    let (mut rd, mut wr) = tokio::io::split(s);
    let input = data.clone();
    let writer = tokio::spawn(async move {
        for chunk in input.chunks(60_000) {
            wr.write_all(&encode_record(RecordKind::Stdin, chunk).unwrap())
                .await
                .unwrap();
        }
        wr.write_all(&encode_record(RecordKind::StdinEof, b"").unwrap())
            .await
            .unwrap();
        wr
    });
    let out = collect(&mut rd).await;
    drop(writer.await.unwrap());
    assert!(out.stdout == data, "{label}: bytes differ");
    assert_eq!(out.exit.unwrap().code, Some(0), "{label}");

    let mut p = params(&["/bin/sh", "-c", STUBBORN]);
    p.kill_grace_ms = Some(100);
    let mut s = connect().await;
    codec::write_frame(&mut s, &p.to_request(1)).await.unwrap();
    let payload = codec::read_frame(&mut s).await.unwrap().unwrap();
    result_of(&serde_json::from_slice(&payload).unwrap());
    let mut dec = RecordDecoder::new(RecordChannel::ProcessOutput);
    let mut line = Vec::new();
    let mut buf = [0u8; 1024];
    while !line.contains(&b'\n') {
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0, "{label}: closed early");
        dec.push(&buf[..n]);
        while let Some(r) = dec.next_record().unwrap() {
            line.extend(r.payload);
        }
    }
    let gc: u32 = String::from_utf8_lossy(&line).trim().parse().unwrap();
    drop(s);
    wait_gone(unix, gc, Duration::from_secs(5)).await;
}

/// In-process `crabd serve` on `listen`, with this build's supervisor.
async fn serve_on(listen: ListenAddr) -> Server {
    let mut config = ServerConfig::new(listen);
    config.supervisor.exe = env!("CARGO_BIN_EXE_crabd").into();
    Server::bind(config, Arc::new(ToolContext::new("/tmp")))
        .await
        .unwrap()
}

/// The handover works for TCP connections too: `crabd serve --tcp` directly, and the Docker
/// TCP transport (`crabd relay` in front of the Unix socket).
#[tokio::test]
async fn tcp_and_relay_connections_hand_over_too() {
    if std::env::var_os(SOCKET_ENV).is_some() {
        return;
    }
    let unix = target().await;
    let server = serve_on(ListenAddr::Tcp("127.0.0.1:0".into())).await;
    let direct = server.tcp_addr().unwrap();
    tokio::spawn(server.run(std::future::pending()));
    let relay = sandcrab_guest::relay::Relay::bind(sandcrab_guest::relay::RelayConfig::new(
        "127.0.0.1:0",
        unix.sock.clone(),
    ))
    .await
    .unwrap();
    let relayed = relay.local_addr().unwrap();
    tokio::spawn(relay.run(std::future::pending()));
    for (addr, label) in [(direct, "tcp"), (relayed, "relay")] {
        let connect = || async move { tokio::net::TcpStream::connect(addr).await.unwrap() };
        handover_checks(&unix, connect, label).await;
    }
}

/// The same on vsock. Needs the `vsock_loopback` transport (CID 1):
/// `sudo modprobe vsock_loopback`.
#[tokio::test]
#[ignore = "needs the vsock_loopback kernel module"]
async fn vsock_connections_hand_over_too() {
    use sandcrab_guest::vsock::{VMADDR_CID_LOCAL, VsockAddr, VsockStream};
    let unix = target().await;
    let port = 20_000 + std::process::id() % 20_000;
    let server = serve_on(ListenAddr::Vsock(port)).await;
    tokio::spawn(server.run(std::future::pending()));
    let connect = || async move {
        VsockStream::connect(VsockAddr::new(VMADDR_CID_LOCAL, port))
            .await
            .unwrap()
    };
    handover_checks(&unix, connect, "vsock").await;
}

// ---------------------------------------------------------------------------------------------
// guest-caps: limits, OOM score, timeout, output cap, stderr modes, logging
// ---------------------------------------------------------------------------------------------

/// Set in the Docker variant that runs `crabd` as a uid with no passwd entry.
const NO_PASSWD_ENV: &str = "SANDCRAB_TEST_EXPECT_NO_PASSWD";

fn message_of(r: &RpcResponse) -> String {
    match &r.payload {
        ResponsePayload::Error(e) => e.message.clone(),
        ResponsePayload::Result(v) => panic!("expected an error, got {v}"),
    }
}

/// Run with `data` written to stdin (then `STDIN_EOF`), return the output.
async fn run_with_stdin(t: &Target, p: &ProcessOpenParams, data: &[u8]) -> Output {
    let mut p = p.clone();
    p.stdin = StdinMode::Pipe;
    let (s, r) = open(t, &p).await;
    result_of(&r);
    let (mut rd, mut wr) = s.into_split();
    let input = data.to_vec();
    let writer = tokio::spawn(async move {
        for chunk in input.chunks(60_000) {
            if wr
                .write_all(&encode_record(RecordKind::Stdin, chunk).unwrap())
                .await
                .is_err()
            {
                return wr;
            }
        }
        let _ = wr
            .write_all(&encode_record(RecordKind::StdinEof, b"").unwrap())
            .await;
        wr
    });
    let out = collect(&mut rd).await;
    drop(writer.await.unwrap());
    out
}

/// 1 MiB of mixed bytes: valid UTF-8, CR/LF, every byte value.
fn mixed_mib() -> Vec<u8> {
    let mut data = Vec::with_capacity(1 << 20);
    let mut i: u32 = 0;
    while data.len() < 1 << 20 {
        match i % 3 {
            0 => data.extend_from_slice("héllo ✓ 漢字\r\n".as_bytes()),
            1 => data.extend((0..=255u8).map(|b| b.wrapping_mul(31).wrapping_add(i as u8))),
            _ => data.extend_from_slice("🦀\n".as_bytes()),
        }
        i += 1;
    }
    data.truncate(1 << 20);
    data
}

/// What `stderr_tail` holds for these last bytes of a longer stream.
fn tail_text(last: &[u8]) -> String {
    let skip = last
        .iter()
        .take(3)
        .take_while(|b| **b & 0xC0 == 0x80)
        .count();
    String::from_utf8_lossy(&last[skip..]).into_owned()
}

/// The value of one row of `/proc/self/limits` (soft, hard).
fn limit_row(limits: &str, name: &str) -> (String, String) {
    let line = limits
        .lines()
        .find(|l| l.starts_with(name))
        .unwrap_or_else(|| panic!("no {name} in {limits}"));
    let mut cols = line[name.len()..].split_whitespace();
    (
        cols.next().unwrap().to_owned(),
        cols.next().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn limits_are_set_before_exec_and_visible_to_the_program() {
    let t = target().await;
    let mut p = params(&["/bin/cat", "/proc/self/limits"]);
    p.limits = Some(ExecLimits {
        nproc: Some(4000),
        data_bytes: Some(64 << 20),
        fsize_bytes: Some(1 << 20),
        nofile: Some(100),
    });
    p.oom_score_adj = Some(500);
    let out = run(&t, &p).await;
    assert_eq!(out.exit.unwrap().code, Some(0));
    let limits = String::from_utf8(out.stdout).unwrap();
    let both = |v: &str| (v.to_owned(), v.to_owned());
    assert_eq!(limit_row(&limits, "Max processes"), both("4000"));
    assert_eq!(limit_row(&limits, "Max data size"), both("67108864"));
    assert_eq!(limit_row(&limits, "Max file size"), both("1048576"));
    assert_eq!(limit_row(&limits, "Max open files"), both("100"));

    let mut p = params(&["/bin/cat", "/proc/self/oom_score_adj"]);
    p.oom_score_adj = Some(500);
    assert_eq!(run(&t, &p).await.stdout, b"500\n");

    // Without limits, the program has the sandbox's own.
    let out = run(&t, &params(&["/bin/cat", "/proc/self/limits"])).await;
    let limits = String::from_utf8(out.stdout).unwrap();
    assert_ne!(limit_row(&limits, "Max open files"), both("100"));
}

#[tokio::test]
async fn each_limit_trips() {
    let t = target().await;
    let limits = |l: ExecLimits| Some(l);

    // fsize: the writer dies of SIGXFSZ at the bound.
    let file = format!("/tmp/fsize-{}", uuid());
    let mut p = params(&[
        "/bin/sh",
        "-c",
        &format!("exec head -c 2000000 /dev/zero > {file}"),
    ]);
    p.limits = limits(ExecLimits {
        fsize_bytes: Some(1 << 20),
        ..ExecLimits::default()
    });
    let exit = run(&t, &p).await.exit.unwrap();
    assert_eq!((exit.code, exit.signal), (None, Some(25)), "{exit:?}");
    assert!(exit.tree_clean);
    let size = run(
        &t,
        &params(&["/bin/sh", "-c", &format!("wc -c < {file}; rm -f {file}")]),
    )
    .await;
    assert_eq!(String::from_utf8_lossy(&size.stdout).trim(), "1048576");

    // nofile: a descriptor at or above the bound cannot be opened.
    let dup_70 = |n: u64| {
        let mut p = params(&["/bin/bash", "-c", "exec 70</dev/null"]);
        p.limits = limits(ExecLimits {
            nofile: Some(n),
            ..ExecLimits::default()
        });
        p
    };
    assert_ne!(run(&t, &dup_70(64)).await.exit.unwrap().code, Some(0));
    assert_eq!(run(&t, &dup_70(100)).await.exit.unwrap().code, Some(0));

    // nproc counts every process of the uid; with 1 not even the program can be started.
    let mut p = params(&["/bin/true"]);
    p.limits = limits(ExecLimits {
        nproc: Some(1),
        ..ExecLimits::default()
    });
    let (code, data) = error_of(&open(&t, &p).await.1);
    assert_eq!(code, codes::SPAWN_FAILED);
    assert_eq!(data, json!({"errno": "EAGAIN", "what": "program"}));

    // data: an allocation past the bound fails (the shell runs out of memory).
    let mut p = params(&[
        "/bin/sh",
        "-c",
        "x=$(head -c 100000000 /dev/zero | tr '\\0' a); echo ${#x}",
    ]);
    p.limits = limits(ExecLimits {
        data_bytes: Some(32 << 20),
        ..ExecLimits::default()
    });
    let out = run(&t, &p).await;
    assert_ne!(out.stdout, b"100000000\n");
    assert!(out.exit.unwrap().tree_clean);
}

#[tokio::test]
async fn a_limit_above_the_sandbox_is_invalid_params() {
    let t = target().await;
    let mut p = params(&["/bin/true"]);
    p.limits = Some(ExecLimits {
        nofile: Some(1 << 40),
        ..ExecLimits::default()
    });
    let r = open(&t, &p).await.1;
    assert_eq!(error_of(&r).0, codes::INVALID_PARAMS);
    assert!(
        message_of(&r).contains("limits.nofile"),
        "{}",
        message_of(&r)
    );
}

#[tokio::test]
async fn timeout_stops_the_tree_and_is_told_apart_from_cancel() {
    let t = target().await;
    // A program that dies on TERM.
    let mut p = params(&["/bin/sleep", "1000"]);
    p.timeout_ms = Some(300);
    p.kill_grace_ms = Some(2000);
    let started = Instant::now();
    let exit = run(&t, &p).await.exit.unwrap();
    assert!(
        exit.timed_out && !exit.cancelled && exit.tree_clean,
        "{exit:?}"
    );
    assert_eq!(exit.signal, Some(15));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(exit.duration_ms >= 300);

    // A tree that ignores TERM: forced after the grace, grandchild included.
    let mut p = params(&["/bin/sh", "-c", STUBBORN]);
    p.timeout_ms = Some(500);
    p.kill_grace_ms = Some(100);
    let (mut s, r) = open(&t, &p).await;
    result_of(&r);
    let gc: u32 = first_line(&mut s).await.parse().unwrap();
    let exit = collect(&mut s).await.exit.unwrap();
    assert!(
        exit.timed_out && !exit.cancelled && exit.tree_clean,
        "{exit:?}"
    );
    assert_eq!(exit.signal, Some(9));
    assert!(!alive(&t, gc).await, "grandchild {gc} survived the timeout");

    // Within the bound: not timed out.
    let mut p = params(&["/bin/true"]);
    p.timeout_ms = Some(10_000);
    let exit = run(&t, &p).await.exit.unwrap();
    assert!(!exit.timed_out && exit.code == Some(0));

    // A cancel before the bound is a cancel, not a timeout.
    let mut p = params(&["/bin/sleep", "1000"]);
    p.timeout_ms = Some(10_000);
    let (mut s, r) = open(&t, &p).await;
    result_of(&r);
    let res = cancel(&t, &p.process_id, 100).await;
    assert!(res.was_running && res.tree_clean);
    let exit = collect(&mut s).await.exit.unwrap();
    assert!(exit.cancelled && !exit.timed_out, "{exit:?}");
}

#[tokio::test]
async fn output_cap_kill_cuts_at_the_cap_and_stops_the_tree() {
    let t = target().await;
    let mut p = params(&["/usr/bin/yes", "0123456789"]);
    p.output_cap = Some(OutputCap {
        max_bytes: 100_001,
        on_overflow: Overflow::Kill,
    });
    p.kill_grace_ms = Some(100);
    let out = run(&t, &p).await;
    assert_eq!(out.stdout.len(), 100_001);
    assert!(out.stdout.starts_with(b"0123456789\n0123456789\n"));
    let exit = out.exit.unwrap();
    assert!(
        exit.overflow && exit.tree_clean && !exit.timed_out && !exit.cancelled,
        "{exit:?}"
    );
    assert!(exit.signal.is_some(), "{exit:?}");

    // Exactly the cap is not an overflow.
    let mut p = params(&["/usr/bin/printf", "abc"]);
    p.output_cap = Some(OutputCap {
        max_bytes: 3,
        on_overflow: Overflow::Kill,
    });
    let out = run(&t, &p).await;
    assert_eq!(out.stdout, b"abc");
    let exit = out.exit.unwrap();
    assert!(!exit.overflow && exit.code == Some(0), "{exit:?}");

    // With `merge`, stderr counts too.
    let mut p = params(&["/bin/sh", "-c", "echo out; exec yes err >&2"]);
    p.stderr = StderrMode::Merge;
    p.output_cap = Some(OutputCap {
        max_bytes: 50_000,
        on_overflow: Overflow::Kill,
    });
    let out = run(&t, &p).await;
    assert_eq!(out.stdout.len(), 50_000);
    assert!(out.stdout.starts_with(b"out\nerr\nerr\n"));
    assert!(out.exit.unwrap().overflow);
}

#[tokio::test]
async fn output_cap_tail_keeps_the_last_bytes_and_sends_them_at_the_end() {
    let t = target().await;
    let mut p = params(&["/usr/bin/seq", "1", "200000"]);
    p.output_cap = Some(OutputCap {
        max_bytes: 1000,
        on_overflow: Overflow::Tail,
    });
    let out = run(&t, &p).await;
    let all: String = (1..=200_000).map(|i| format!("{i}\n")).collect();
    assert_eq!(out.stdout, all.as_bytes()[all.len() - 1000..]);
    let exit = out.exit.unwrap();
    assert!(
        exit.overflow && exit.code == Some(0) && exit.tree_clean,
        "{exit:?}"
    );

    // Nothing is streamed while the program runs; under the cap there is no overflow.
    let mut p = params(&["/bin/sh", "-c", "echo early; sleep 0.6; echo late"]);
    p.output_cap = Some(OutputCap {
        max_bytes: 100,
        on_overflow: Overflow::Tail,
    });
    let (mut s, r) = open(&t, &p).await;
    result_of(&r);
    let mut b = [0u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(400), s.read(&mut b))
            .await
            .is_err(),
        "a record before the program ended"
    );
    let out = collect(&mut s).await;
    assert_eq!(out.stdout, b"early\nlate\n");
    assert!(!out.exit.unwrap().overflow);
}

#[tokio::test]
async fn stderr_tail_merge_and_stream() {
    let t = target().await;
    let script = "echo o1; echo e1 >&2; echo o2; echo e2 >&2";

    // tail (default): nothing streamed, the tail in EXIT.
    let out = run(&t, &params(&["/bin/sh", "-c", script])).await;
    assert_eq!(out.stdout, b"o1\no2\n");
    assert!(out.stderr.is_empty());
    assert_eq!(out.exit.unwrap().stderr_tail, "e1\ne2\n");

    // tail, shorter than the output; a character cut in half at the front is dropped.
    let mut p = params(&["/bin/sh", "-c", "printf 'xéé' >&2"]);
    p.stderr_tail_bytes = Some(3);
    assert_eq!(run(&t, &p).await.exit.unwrap().stderr_tail, "é");
    p.stderr_tail_bytes = Some(0);
    p.process_id = uuid();
    assert_eq!(run(&t, &p).await.exit.unwrap().stderr_tail, "");

    // stream: STDERR records, and the tail too.
    let mut p = params(&["/bin/sh", "-c", script]);
    p.stderr = StderrMode::Stream;
    let out = run(&t, &p).await;
    assert_eq!(out.stdout, b"o1\no2\n");
    assert_eq!(out.stderr, b"e1\ne2\n");
    assert_eq!(out.exit.unwrap().stderr_tail, "e1\ne2\n");

    // merge: one stream in the order of the writes, no STDERR, no tail.
    let interleave = "i=0; while [ $i -lt 2000 ]; do echo o$i; echo e$i >&2; i=$((i+1)); done";
    let mut p = params(&["/bin/sh", "-c", interleave]);
    p.stderr = StderrMode::Merge;
    let out = run(&t, &p).await;
    let want: String = (0..2000).map(|i| format!("o{i}\ne{i}\n")).collect();
    assert!(out.stdout == want.as_bytes(), "merged order differs");
    assert!(out.stderr.is_empty());
    let exit = out.exit.unwrap();
    assert_eq!((exit.stderr_tail.as_str(), exit.code), ("", Some(0)));
}

#[tokio::test]
async fn one_mib_through_each_mode() {
    let t = target().await;
    let data = mixed_mib();
    let to_stderr = || params(&["/bin/sh", "-c", "exec cat >&2"]);

    // stderr `stream`: no cap on the records; the tail is the last 4096 bytes.
    let mut p = to_stderr();
    p.stderr = StderrMode::Stream;
    let out = run_with_stdin(&t, &p, &data).await;
    assert!(out.stderr == data, "stream: bytes differ");
    assert!(out.stdout.is_empty());
    assert_eq!(
        out.exit.unwrap().stderr_tail,
        tail_text(&data[data.len() - 4096..])
    );

    // stderr `merge`: all of it as STDOUT.
    let mut p = to_stderr();
    p.stderr = StderrMode::Merge;
    let out = run_with_stdin(&t, &p, &data).await;
    assert!(out.stdout == data, "merge: bytes differ");

    // stderr `tail`: the last 64 KiB only.
    let mut p = to_stderr();
    p.stderr_tail_bytes = Some(65_536);
    let out = run_with_stdin(&t, &p, &data).await;
    assert!(out.stderr.is_empty() && out.stdout.is_empty());
    assert_eq!(
        out.exit.unwrap().stderr_tail,
        tail_text(&data[data.len() - 65_536..])
    );

    // stdout with a `kill` cap above it and a `tail` cap above it: identical, no overflow.
    for on_overflow in [Overflow::Kill, Overflow::Tail] {
        let mut p = params(&["/bin/cat"]);
        p.output_cap = Some(OutputCap {
            max_bytes: 2 << 20,
            on_overflow,
        });
        let out = run_with_stdin(&t, &p, &data).await;
        assert!(out.stdout == data, "{on_overflow:?}: bytes differ");
        assert!(!out.exit.unwrap().overflow);
    }

    // A `tail` cap below it: exactly the last bytes.
    let mut p = params(&["/bin/cat"]);
    p.output_cap = Some(OutputCap {
        max_bytes: 300_000,
        on_overflow: Overflow::Tail,
    });
    let out = run_with_stdin(&t, &p, &data).await;
    assert!(
        out.stdout == data[data.len() - 300_000..],
        "tail: bytes differ"
    );
    assert!(out.exit.unwrap().overflow);
}

/// The Docker variant runs `crabd` as a uid:gid with no passwd entry and no workspace
/// mounts: a program still runs, and HOME comes only from the given env.
#[tokio::test]
async fn runs_as_a_uid_without_a_passwd_entry() {
    let t = target().await;
    let script = r#"id -u; echo "$HOME"; cd "$HOME" && pwd
if grep -q "^[^:]*:[^:]*:$(id -u):" /etc/passwd; then echo passwd; else echo no-passwd; fi"#;
    let mut p = params(&["/bin/sh", "-c", script]);
    p.env.insert("HOME".into(), "/tmp".into());
    let out = run(&t, &p).await;
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(out.exit.unwrap().code, Some(0), "{text}");
    assert_eq!(lines[1..3], ["/tmp", "/tmp"], "{text}");
    if std::env::var_os(NO_PASSWD_ENV).is_some() {
        assert_eq!(lines[3], "no-passwd", "{text}");
    }
}

/// Env values, stdin and argv beyond argv[0] never reach the log, even at trace level, nor
/// error data. Runs its own `crabd serve` binary to own its log.
#[tokio::test]
async fn secrets_never_reach_a_trace_level_log() {
    if std::env::var_os(SOCKET_ENV).is_some() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("crabd.sock");
    let log_path = dir.path().join("crabd.log");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_crabd"))
        .args(["serve", "--default-cwd", "/tmp", "--unix"])
        .arg(&sock)
        .env("RUST_LOG", "trace")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log_path).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + LONG;
    while UnixStream::connect(&sock).await.is_err() {
        assert!(Instant::now() < deadline, "crabd serve did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let t = Target { sock, _local: None };

    let env_marker = "envsecret-5b1e0c9a";
    let stdin_marker = "stdinsecret-8d2f7e41";
    let argv_marker = "argvsecret-c3a96b07";
    let with_secrets = |a: &[&str]| {
        let mut p = params(a);
        p.env.insert("TOKEN".into(), env_marker.into());
        p
    };

    // A run that reads stdin, with every stderr mode and a failing exit.
    for mode in [StderrMode::Tail, StderrMode::Stream, StderrMode::Merge] {
        let mut p = with_secrets(&[
            "/bin/sh",
            "-c",
            "cat; echo \"$TOKEN\" >&2; exit 1",
            "sh",
            argv_marker,
        ]);
        p.stderr = mode;
        let out = run_with_stdin(&t, &p, stdin_marker.as_bytes()).await;
        assert_eq!(
            out.stdout.get(..stdin_marker.len()),
            Some(stdin_marker.as_bytes())
        );
        assert_eq!(out.exit.unwrap().code, Some(1));
    }
    // A program that cannot start, a refused limit, a timeout: error data stays clean too.
    for p in [with_secrets(&["/nonexistent/prog", argv_marker]), {
        let mut p = with_secrets(&["/bin/true", argv_marker]);
        p.limits = Some(ExecLimits {
            nofile: Some(1 << 40),
            ..ExecLimits::default()
        });
        p
    }] {
        let r = open(&t, &p).await.1;
        let text = serde_json::to_string(&r).unwrap();
        for m in [env_marker, argv_marker] {
            assert!(!text.contains(m), "{m} in {text}");
        }
    }
    let mut p = with_secrets(&["/bin/sh", "-c", "sleep 1000", "sh", argv_marker]);
    p.timeout_ms = Some(100);
    assert!(run(&t, &p).await.exit.unwrap().timed_out);

    child.start_kill().unwrap();
    let _ = child.wait().await;
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("program started"),
        "no supervisor trace in:\n{log}"
    );
    assert!(
        log.contains("/nonexistent/prog") || log.contains("could not start"),
        "{log}"
    );
    for m in [env_marker, stdin_marker, argv_marker] {
        assert!(!log.contains(m), "{m} in the log:\n{log}");
    }
}
