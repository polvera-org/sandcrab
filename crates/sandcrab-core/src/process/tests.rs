use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use sandcrab_protocol::{SpawnTarget, StderrMode, codes};
use tokio::sync::Semaphore;

use super::*;
use crate::testing::{MockConnector, MockCrabd};

fn params(argv: &[&str]) -> ProcessOpenParams {
    ProcessOpenParams::new(
        uuid::Uuid::new_v4().to_string(),
        argv.iter().map(|s| (*s).to_owned()).collect(),
    )
}

fn piped(argv: &[&str]) -> ProcessOpenParams {
    ProcessOpenParams {
        stdin: StdinMode::Pipe,
        ..params(argv)
    }
}

const T: Duration = Duration::from_secs(10);

/// Read every event; returns stdout, stderr and the exit (which must come last).
async fn drain(out: &mut ProcessOutput) -> Result<(Vec<u8>, Vec<u8>, ProcessExit)> {
    let (mut so, mut se) = (Vec::new(), Vec::new());
    loop {
        match out.next().await? {
            Some(ProcessEvent::Stdout(d)) => so.extend(d),
            Some(ProcessEvent::Stderr(d)) => se.extend(d),
            Some(ProcessEvent::Exit(e)) => {
                assert!(out.next().await?.is_none(), "exit must be last");
                return Ok((so, se, e));
            }
            None => panic!("stream ended before its exit"),
        }
    }
}

fn mixed_bytes(n: usize) -> Vec<u8> {
    // Every byte value, CRLF and invalid UTF-8 included.
    (0..n).map(|i| ((i * 7 + i / 3) % 256) as u8).collect()
}

#[tokio::test]
async fn one_mib_through_stdin_and_stdout_in_order() {
    let mock = MockCrabd::default();
    let conn = MockConnector::new(mock.clone());
    let ps = open_process(&conn, &piped(&["echo"]), T).await.unwrap();
    assert_eq!(ps.pid(), 4242);
    let (mut stdin, mut out) = ps.into_split();
    let data = mixed_bytes(1 << 20);
    let sent = data.clone();
    let writer = tokio::spawn(async move {
        // Uneven writes: records never cross a write's boundaries.
        for chunk in sent.chunks(100_003) {
            stdin.write(chunk).await.unwrap();
        }
        stdin.close().await.unwrap();
        stdin.close().await.unwrap(); // idempotent
        stdin
    });
    let (so, se, exit) = drain(&mut out).await.unwrap();
    assert_eq!(so.len(), data.len());
    assert!(so == data, "stdout differs from stdin");
    assert!(se.is_empty());
    assert_eq!(exit.code, Some(0));
    let mut stdin = writer.await.unwrap();
    assert!(matches!(
        stdin.write(b"late").await,
        Err(SandboxError::InvalidConfig(_))
    ));
}

#[tokio::test]
async fn output_is_ordered_and_exit_comes_last() {
    let conn = MockConnector::new(MockCrabd::default());
    let n = (1 << 20) + 17;
    let mut ps = open_process(&conn, &params(&["emit", &n.to_string()]), T)
        .await
        .unwrap();
    let (so, _, exit) = drain(ps.output()).await.unwrap();
    let want: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
    assert!(so == want);
    assert_eq!(exit.code, Some(0));
    assert!(ps.output().is_finished());

    // stdout and stderr records keep the order they were sent in.
    let mut ps = open_process(
        &conn,
        &ProcessOpenParams {
            stderr: StderrMode::Stream,
            ..params(&["mixed"])
        },
        T,
    )
    .await
    .unwrap();
    let mut got = Vec::new();
    while let Some(ev) = ps.output().next().await.unwrap() {
        got.push(ev);
    }
    assert_eq!(got[0], ProcessEvent::Stdout(b"a".to_vec()));
    assert_eq!(got[1], ProcessEvent::Stderr(b"b".to_vec()));
    assert_eq!(got[2], ProcessEvent::Stdout(b"c".to_vec()));
    assert!(matches!(&got[3], ProcessEvent::Exit(e) if e.stderr_tail == "b"));
}

#[tokio::test]
async fn a_paused_reader_holds_the_writer_back() {
    let mock = MockCrabd::default();
    let conn = MockConnector::new(mock.clone());
    let total = 8 << 20;
    let mut ps = open_process(&conn, &params(&["emit", &total.to_string()]), T)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Bound: the transport buffer (64 KiB duplex) plus one record in the mock's write.
    let written = mock.processes.stdout_written.load(Ordering::SeqCst);
    assert!(
        written <= 256 * 1024,
        "{written} bytes written while paused"
    );
    let (so, _, exit) = drain(ps.output()).await.unwrap();
    assert_eq!(so.len(), total);
    assert_eq!(exit.code, Some(0));
}

#[tokio::test]
async fn stdin_rules() {
    let conn = MockConnector::new(MockCrabd::default());
    // stdin: null refuses writes without sending anything; close is a no-op.
    let mut ps = open_process(&conn, &params(&["exit", "3"]), T)
        .await
        .unwrap();
    assert!(matches!(
        ps.stdin().write(b"x").await,
        Err(SandboxError::InvalidConfig(_))
    ));
    ps.stdin().close().await.unwrap();
    let (_, _, exit) = drain(ps.output()).await.unwrap();
    assert_eq!(exit.code, Some(3));
    assert_eq!(exit.stderr_tail, "bye");

    // EOF without any input: the program sees end-of-file and exits.
    let mut ps = open_process(&conn, &piped(&["echo"]), T).await.unwrap();
    ps.stdin().write(b"").await.unwrap();
    ps.stdin().close().await.unwrap();
    let (so, _, exit) = drain(ps.output()).await.unwrap();
    assert!(so.is_empty());
    assert_eq!(exit.code, Some(0));
}

#[tokio::test]
async fn could_not_start_is_typed() {
    let conn = MockConnector::new(MockCrabd::default());
    let err = open_process(&conn, &params(&["missing"]), T)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SandboxError::Protocol(ProtocolError::SpawnFailed { errno, what })
            if errno == "ENOENT" && *what == SpawnTarget::Program),
        "{err:?}"
    );
    let err = open_process(&conn, &params(&["no-cwd"]), T)
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            SandboxError::Protocol(ProtocolError::SpawnFailed {
                what: SpawnTarget::Cwd,
                ..
            })
        ),
        "{err:?}"
    );
    let err = open_process(&conn, &params(&["limit"]), T)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SandboxError::Protocol(e) if e.code() == codes::LIMIT_EXCEEDED),
        "{err:?}"
    );
}

#[tokio::test]
async fn invalid_params_connect_nothing() {
    let conn = MockConnector::new(MockCrabd::default());
    let mut p = params(&["echo"]);
    p.process_id = "not-a-uuid".into();
    let err = open_process(&conn, &p, T).await.unwrap_err();
    assert!(
        matches!(err, SandboxError::Protocol(ProtocolError::InvalidParams(_))),
        "{err:?}"
    );
    assert_eq!(conn.connects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unavailable_when_nothing_could_be_sent() {
    let conn =
        MockConnector::new(MockCrabd::default()).failing_first(Arc::new(AtomicU32::new(u32::MAX)));
    let err = open_process(&conn, &params(&["echo"]), T)
        .await
        .unwrap_err();
    assert!(matches!(err, SandboxError::Unavailable(_)), "{err:?}");
}

#[tokio::test]
async fn lost_when_the_connection_ends_without_exit() {
    let mock = MockCrabd::default();
    let conn = MockConnector::new(mock.clone());
    // Before the ack: the request was written, the program may have run.
    let err = open_process(&conn, &params(&["hangup-before-ack"]), T)
        .await
        .unwrap_err();
    assert!(matches!(err, SandboxError::Lost(_)), "{err:?}");

    // After the ack, mid-process.
    let mut ps = open_process(&conn, &params(&["hangup"]), T).await.unwrap();
    let err = ps.output().next().await.unwrap_err();
    assert!(matches!(err, SandboxError::Lost(_)), "{err:?}");
    // And it stays lost.
    assert!(matches!(
        ps.output().next().await,
        Err(SandboxError::Lost(_))
    ));
}

#[tokio::test]
async fn dropping_the_stream_closes_the_connection() {
    let mock = MockCrabd::default();
    let conn = MockConnector::new(mock.clone());
    let ps = open_process(&conn, &params(&["sleep"]), T).await.unwrap();
    assert_eq!(mock.processes.live(), 1);
    let (stdin, out) = ps.into_split();
    drop(stdin);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(mock.processes.live(), 1, "one half still holds it open");
    drop(out);
    wait_until(|| mock.processes.live() == 0).await;
    assert_eq!(mock.processes.closed_by_host.load(Ordering::SeqCst), 1);
}

async fn wait_until(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "condition not met in time");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn client_for(conn: &MockConnector) -> Arc<CrabdClient> {
    let c = CrabdClient::connect(conn).await.unwrap();
    // The mock has started serving this connection (and read its knobs).
    c.hello().await.unwrap();
    Arc::new(c)
}

#[tokio::test]
async fn cancel_a_running_process() {
    let mock = MockCrabd::default();
    let conn = MockConnector::new(mock.clone());
    let client = client_for(&conn).await;
    let table = ProcessTable::new();
    let p = params(&["sleep"]);
    let mut ps = table.open(&conn, &p, T).await.unwrap();
    assert!(table.contains(&p.process_id));
    let out = tokio::spawn(async move { drain(ps.output()).await.map(|r| r.2) });
    let r = table.cancel(&client, &p.process_id, Some(0)).await.unwrap();
    assert!(r.was_running && r.tree_clean);
    let exit = out.await.unwrap().unwrap();
    assert!(exit.cancelled && exit.signal == Some(15) && exit.code.is_none());
    // Idempotent: a second cancel finds nothing running.
    let r = table.cancel(&client, &p.process_id, None).await.unwrap();
    assert!(!r.was_running);
    wait_until(|| !table.contains(&p.process_id)).await;
}

#[tokio::test]
async fn duplicate_process_id_is_refused() {
    let conn = MockConnector::new(MockCrabd::default());
    let table = ProcessTable::new();
    let p = params(&["sleep"]);
    let _ps = table.open(&conn, &p, T).await.unwrap();
    let err = table.open(&conn, &p, T).await.unwrap_err();
    assert!(
        matches!(
            err,
            SandboxError::AlreadyExists {
                kind: "process",
                ..
            }
        ),
        "{err:?}"
    );
}

/// A connector that waits for a permit before each connect.
#[derive(Debug)]
struct Gated {
    inner: MockConnector,
    gate: Arc<Semaphore>,
}

#[async_trait]
impl Connector for Gated {
    async fn connect(&self) -> std::io::Result<BoxedStream> {
        self.gate.acquire().await.expect("open").forget();
        self.inner.connect().await
    }
}

#[tokio::test]
async fn cancel_before_the_request_is_written_starts_nothing() {
    let mock = MockCrabd::default();
    let inner = MockConnector::new(mock.clone());
    let client = client_for(&inner).await;
    let conn = Gated {
        inner,
        gate: Arc::new(Semaphore::new(0)),
    };
    let table = ProcessTable::new();
    let p = params(&["sleep"]);
    let opening = {
        let (table, p) = (table.clone(), p.clone());
        let conn = Gated {
            inner: conn.inner.clone(),
            gate: Arc::clone(&conn.gate),
        };
        tokio::spawn(async move { table.open(&conn, &p, T).await.map(|_| ()) })
    };
    wait_until(|| table.contains(&p.process_id)).await;
    let r = table.cancel(&client, &p.process_id, None).await.unwrap();
    assert!(!r.was_running && r.tree_clean);
    conn.gate.add_permits(1);
    let err = opening.await.unwrap().unwrap_err();
    assert!(
        matches!(err, SandboxError::Protocol(ProtocolError::Cancelled(_))),
        "{err:?}"
    );
    assert_eq!(mock.processes.open_requests.load(Ordering::SeqCst), 0);
    assert_eq!(mock.processes.cancels.load(Ordering::SeqCst), 0);
    assert!(!table.contains(&p.process_id));
}

#[tokio::test]
async fn cancel_remembered_by_the_guest_before_start() {
    let mock = MockCrabd::default();
    let conn = MockConnector::new(mock.clone());
    let client = client_for(&conn).await;
    let table = ProcessTable::new();
    let p = params(&["slow-start", "300"]);
    let opening = {
        let (table, p, conn) = (table.clone(), p.clone(), conn.clone());
        tokio::spawn(async move { table.open(&conn, &p, T).await.map(|_| ()) })
    };
    wait_until(|| mock.processes.contains(&p.process_id)).await;
    let r = table.cancel(&client, &p.process_id, None).await.unwrap();
    assert!(!r.was_running);
    let err = opening.await.unwrap().unwrap_err();
    assert!(
        matches!(&err, SandboxError::Protocol(e) if e.code() == codes::CANCELLED),
        "{err:?}"
    );
    assert_eq!(mock.processes.opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancel_that_overtakes_the_open_is_remembered_by_the_guest() {
    let mock = MockCrabd::default();
    let conn = MockConnector::new(mock.clone());
    let client = client_for(&conn).await;
    // From now on the guest reads a new connection's first frame late, so the cancel on
    // the tool connection arrives before it knows the process.
    mock.first_frame_delay_ms.store(300, Ordering::SeqCst);
    let table = ProcessTable::new();
    let p = params(&["sleep"]);
    let opening = {
        let (table, p, conn) = (table.clone(), p.clone(), conn.clone());
        tokio::spawn(async move { table.open(&conn, &p, T).await.map(|_| ()) })
    };
    wait_until(|| {
        table
            .inner
            .lock()
            .unwrap()
            .get(&p.process_id)
            .is_some_and(|e| e.written)
    })
    .await;
    let r = table.cancel(&client, &p.process_id, Some(0)).await.unwrap();
    assert!(!r.was_running, "the guest did not know the process yet");
    let err = opening.await.unwrap().unwrap_err();
    assert!(
        matches!(&err, SandboxError::Protocol(e) if e.code() == codes::CANCELLED),
        "{err:?}"
    );
    assert_eq!(mock.processes.opened.load(Ordering::SeqCst), 0);
    assert_eq!(mock.processes.cancels.load(Ordering::SeqCst), 1);
}
