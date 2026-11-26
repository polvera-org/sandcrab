use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

use super::*;
use crate::testing::{MockConnector, MockCrabd, MockForwards};

/// Echoes every byte; half-closes after the program's EOF.
struct Echo;

#[async_trait]
impl ForwardTarget for Echo {
    async fn connect(&self, _: SandboxId, _: u16) -> io::Result<BoxedStream> {
        let (a, b) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(b);
            let _ = tokio::io::copy(&mut r, &mut w).await;
            let _ = w.shutdown().await;
        });
        Ok(Box::new(a))
    }
}

/// Reads until EOF, then answers the byte count and closes (upload-until-EOF).
struct Count;

#[async_trait]
impl ForwardTarget for Count {
    async fn connect(&self, _: SandboxId, _: u16) -> io::Result<BoxedStream> {
        let (a, mut b) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = b.read_to_end(&mut buf).await;
            let _ = b.write_all(buf.len().to_string().as_bytes()).await;
            let _ = b.shutdown().await;
        });
        Ok(Box::new(a))
    }
}

struct Refuse(AtomicU32);

#[async_trait]
impl ForwardTarget for Refuse {
    async fn connect(&self, _: SandboxId, _: u16) -> io::Result<BoxedStream> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "no target",
        ))
    }
}

/// Holds each target connection open and silent (keeps the slot taken).
struct Hold(std::sync::Mutex<Vec<DuplexStream>>);

#[async_trait]
impl ForwardTarget for Hold {
    async fn connect(&self, _: SandboxId, _: u16) -> io::Result<BoxedStream> {
        let (a, b) = tokio::io::duplex(1024);
        self.0.lock().unwrap().push(b);
        Ok(Box::new(a))
    }
}

/// Slow to connect.
struct Slow(Duration);

#[async_trait]
impl ForwardTarget for Slow {
    async fn connect(&self, s: SandboxId, p: u16) -> io::Result<BoxedStream> {
        tokio::time::sleep(self.0).await;
        Echo.connect(s, p).await
    }
}

/// Answers some bytes, then fails.
struct Breaks;

#[async_trait]
impl ForwardTarget for Breaks {
    async fn connect(&self, _: SandboxId, _: u16) -> io::Result<BoxedStream> {
        let (a, mut b) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            let _ = b.write_all(b"partial").await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(b); // the target goes away without EOF on a read in progress
        });
        Ok(Box::new(ErrAfterEof(a)))
    }
}

/// A stream whose EOF is reported as an error (a reset).
struct ErrAfterEof(DuplexStream);

impl AsyncRead for ErrAfterEof {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let before = buf.filled().len();
        match std::pin::Pin::new(&mut self.0).poll_read(cx, buf) {
            std::task::Poll::Ready(Ok(())) if buf.filled().len() == before => {
                std::task::Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset")))
            }
            other => other,
        }
    }
}

impl AsyncWrite for ErrAfterEof {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

fn quick() -> ForwardConfig {
    ForwardConfig {
        reconnect_min: Duration::from_millis(10),
        reconnect_max: Duration::from_millis(50),
        ..ForwardConfig::default()
    }
}

struct Setup {
    mock: MockCrabd,
    fwd: Arc<MockForwards>,
    acceptor: ForwardAcceptor,
}

async fn setup(
    specs: &[ForwardSpec],
    target: Arc<dyn ForwardTarget>,
    config: ForwardConfig,
) -> Setup {
    let ports: Vec<u16> = specs.iter().map(|f| f.guest_port).collect();
    let mock = MockCrabd {
        forwards: Arc::new(MockForwards::new(&ports)),
        ..MockCrabd::default()
    };
    let acceptor = ForwardAcceptor::start(
        SandboxId::new(),
        Arc::new(MockConnector::new(mock.clone())),
        specs,
        target,
        config,
        None,
    )
    .unwrap();
    assert!(acceptor.wait_listening(Duration::from_secs(5)).await);
    Setup {
        fwd: Arc::clone(&mock.forwards),
        mock,
        acceptor,
    }
}

async fn read_all(s: &mut DuplexStream) -> Vec<u8> {
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out))
        .await
        .expect("closed in time")
        .unwrap_or(0);
    out
}

async fn wait_until(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "condition not met in time");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn relays_both_ways_with_half_close() {
    let s = setup(&[ForwardSpec::new(5432)], Arc::new(Echo), quick()).await;
    for _ in 0..3 {
        let mut p = s.fwd.connect(5432).unwrap();
        p.write_all(b"ping").await.unwrap();
        p.shutdown().await.unwrap();
        assert_eq!(read_all(&mut p).await, b"ping");
    }
    assert_eq!(s.fwd.stats_of(5432).accepted, 3);
    assert_eq!(s.fwd.stats_of(5432).refused, Default::default());

    // Large, in both directions at once.
    let mut p = s.fwd.connect(5432).unwrap();
    let data: Vec<u8> = (0..(1 << 20)).map(|i| (i % 253) as u8).collect();
    let (mut r, mut w) = tokio::io::split(&mut p);
    let send = async {
        w.write_all(&data).await.unwrap();
        w.shutdown().await.unwrap();
    };
    let mut back = Vec::new();
    let recv = r.read_to_end(&mut back);
    let ((), n) = tokio::join!(send, recv);
    assert_eq!(n.unwrap(), data.len());
    assert!(back == data);
    s.acceptor.shutdown().await;
}

#[tokio::test]
async fn upload_until_eof_gets_its_reply() {
    let s = setup(&[ForwardSpec::new(80)], Arc::new(Count), quick()).await;
    let mut p = s.fwd.connect(80).unwrap();
    p.write_all(&vec![7u8; 300_000]).await.unwrap();
    p.shutdown().await.unwrap();
    assert_eq!(read_all(&mut p).await, b"300000");
}

#[tokio::test]
async fn failing_target_rejects() {
    let target = Arc::new(Refuse(AtomicU32::new(0)));
    let s = setup(&[ForwardSpec::new(80)], target.clone(), quick()).await;
    let mut p = s.fwd.connect(80).unwrap();
    assert!(
        read_all(&mut p).await.is_empty(),
        "refused connection is closed"
    );
    wait_until(|| s.fwd.stats_of(80).refused.rejected == 1).await;
    assert_eq!(target.0.load(Ordering::SeqCst), 1);
    assert_eq!(s.fwd.stats_of(80).accepted, 0);
    // A port that is not forwarded is refused by the "kernel".
    assert_eq!(
        s.fwd.connect(81).unwrap_err().kind(),
        io::ErrorKind::ConnectionRefused
    );
}

#[tokio::test]
async fn caps_per_forward_and_per_sandbox() {
    let hold = Arc::new(Hold(std::sync::Mutex::default()));
    let s = setup(
        &[ForwardSpec {
            guest_port: 80,
            max_connections: Some(1),
        }],
        hold.clone(),
        quick(),
    )
    .await;
    let _first = s.fwd.connect(80).unwrap();
    wait_until(|| s.fwd.stats_of(80).accepted == 1).await;
    let mut second = s.fwd.connect(80).unwrap();
    assert!(read_all(&mut second).await.is_empty());
    wait_until(|| s.fwd.stats_of(80).refused.rejected == 1).await;
    assert_eq!(hold.0.lock().unwrap().len(), 1, "the target was not asked");

    // Sandbox-wide: one slot shared by two forwards.
    let s = setup(
        &[ForwardSpec::new(80), ForwardSpec::new(81)],
        Arc::new(Hold(std::sync::Mutex::default())),
        ForwardConfig {
            max_connections: 1,
            ..quick()
        },
    )
    .await;
    let _first = s.fwd.connect(80).unwrap();
    wait_until(|| s.fwd.stats_of(80).accepted == 1).await;
    let mut other = s.fwd.connect(81).unwrap();
    assert!(read_all(&mut other).await.is_empty());
    wait_until(|| s.fwd.stats_of(81).refused.rejected == 1).await;
}

#[tokio::test]
async fn slot_is_given_back_when_a_connection_ends() {
    let s = setup(
        &[ForwardSpec {
            guest_port: 80,
            max_connections: Some(1),
        }],
        Arc::new(Echo),
        quick(),
    )
    .await;
    for _ in 0..3 {
        let mut p = s.fwd.connect(80).unwrap();
        p.write_all(b"x").await.unwrap();
        p.shutdown().await.unwrap();
        assert_eq!(read_all(&mut p).await, b"x");
        // The relay ends once both sides are done; then the slot is free again.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(s.fwd.stats_of(80).accepted, 3);
}

#[tokio::test]
async fn an_error_closes_the_guest_connection() {
    let s = setup(&[ForwardSpec::new(80)], Arc::new(Breaks), quick()).await;
    let mut p = s.fwd.connect(80).unwrap();
    // The program gets what arrived, then its connection closes (no hang).
    let got = read_all(&mut p).await;
    assert_eq!(got, b"partial");
}

#[tokio::test]
async fn unclaimed_when_the_target_is_too_slow() {
    let s = setup(
        &[ForwardSpec::new(80)],
        Arc::new(Slow(Duration::from_millis(300))),
        quick(),
    )
    .await;
    s.fwd.unclaimed_timeout_ms.store(50, Ordering::SeqCst);
    let mut p = s.fwd.connect(80).unwrap();
    assert!(read_all(&mut p).await.is_empty());
    wait_until(|| s.fwd.stats_of(80).refused.unclaimed == 1).await;
    // The late accept is refused by the guest; the acceptor keeps serving.
    s.fwd.unclaimed_timeout_ms.store(10_000, Ordering::SeqCst);
    let mut p = s.fwd.connect(80).unwrap();
    p.write_all(b"ok").await.unwrap();
    p.shutdown().await.unwrap();
    assert_eq!(read_all(&mut p).await, b"ok");
}

#[tokio::test]
async fn reconnects_after_the_listen_connection_drops() {
    let s = setup(&[ForwardSpec::new(80)], Arc::new(Echo), quick()).await;
    assert_eq!(s.fwd.listen_connections.load(Ordering::SeqCst), 1);
    s.fwd.drop_listener();
    wait_until(|| s.fwd.listen_connections.load(Ordering::SeqCst) == 2).await;
    assert!(s.acceptor.wait_listening(Duration::from_secs(5)).await);
    let mut p = s.fwd.connect(80).unwrap();
    p.write_all(b"again").await.unwrap();
    p.shutdown().await.unwrap();
    assert_eq!(read_all(&mut p).await, b"again");
}

#[tokio::test]
async fn an_announced_connection_survives_a_dropped_listen_connection() {
    // As in `crabd`: the announcement stays claimable until its unclaimed deadline.
    let s = setup(
        &[ForwardSpec::new(80)],
        Arc::new(Slow(Duration::from_millis(300))),
        quick(),
    )
    .await;
    let mut p = s.fwd.connect(80).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    s.fwd.drop_listener();
    p.write_all(b"still").await.unwrap();
    p.shutdown().await.unwrap();
    assert_eq!(read_all(&mut p).await, b"still");
    assert_eq!(s.fwd.stats_of(80).accepted, 1);
}

#[tokio::test]
async fn no_listener_until_served_and_after_shutdown() {
    let fwd = Arc::new(MockForwards::new(&[80]));
    let mut p = fwd.connect(80).unwrap();
    assert!(read_all(&mut p).await.is_empty());
    wait_until(|| fwd.stats_of(80).refused.no_listener == 1).await;

    let s = setup(
        &[ForwardSpec::new(80)],
        Arc::new(Hold(Default::default())),
        quick(),
    )
    .await;
    let mut open = s.fwd.connect(80).unwrap();
    wait_until(|| s.fwd.stats_of(80).accepted == 1).await;
    let fwd = Arc::clone(&s.fwd);
    s.acceptor.shutdown().await;
    // The relayed connection is cut, and nothing listens any more.
    assert!(read_all(&mut open).await.is_empty());
    wait_until(|| !fwd.has_listener()).await;
    let mut p = fwd.connect(80).unwrap();
    assert!(read_all(&mut p).await.is_empty());
    wait_until(|| fwd.stats_of(80).refused.no_listener == 1).await;
    drop(s.mock);
}

#[tokio::test]
async fn dropping_the_acceptor_stops_it() {
    let s = setup(&[ForwardSpec::new(80)], Arc::new(Echo), quick()).await;
    let fwd = Arc::clone(&s.fwd);
    drop(s.acceptor);
    wait_until(|| !fwd.has_listener()).await;
}

#[tokio::test]
async fn second_listen_connection_is_refused() {
    let s = setup(&[ForwardSpec::new(80)], Arc::new(Echo), quick()).await;
    let ctx = Ctx {
        sandbox: SandboxId::new(),
        connector: Arc::new(MockConnector::new(s.mock.clone())),
        target: Arc::new(Echo),
        config: quick(),
        ports: HashMap::new(),
        total: Arc::new(Semaphore::new(1)),
    };
    let err = open_listen(&ctx).await.err().unwrap();
    assert!(
        matches!(
            err,
            SandboxError::Protocol(ProtocolError::ExecutionFailed(_))
        ),
        "{err:?}"
    );
}

#[test]
fn start_validates() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _g = rt.enter();
    let conn: Arc<dyn Connector> = Arc::new(MockConnector::new(MockCrabd::default()));
    let start = |f: &[ForwardSpec], c: ForwardConfig| {
        ForwardAcceptor::start(
            SandboxId::new(),
            Arc::clone(&conn),
            f,
            Arc::new(Echo),
            c,
            None,
        )
    };
    for (specs, config) in [
        (vec![], quick()),
        (vec![ForwardSpec::new(0)], quick()),
        (vec![ForwardSpec::new(1), ForwardSpec::new(1)], quick()),
        (
            vec![ForwardSpec {
                guest_port: 1,
                max_connections: Some(0),
            }],
            quick(),
        ),
        (
            vec![ForwardSpec::new(1)],
            ForwardConfig {
                max_connections: 0,
                ..quick()
            },
        ),
    ] {
        assert!(matches!(
            start(&specs, config),
            Err(SandboxError::InvalidConfig(_))
        ));
    }
}
