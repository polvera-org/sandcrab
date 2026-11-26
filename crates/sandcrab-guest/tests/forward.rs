//! Forwards against an in-process `crabd` on a temp Unix socket, with this test as the host
//! side: `forward.listen`, `forward.incoming`, `forward.accept`, records, `forward.reject`,
//! the deadline, the caps and the counters. A Docker case (ignored) runs a static `crabd` in a
//! hardened container with network `none`.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use sandcrab_guest::{
    AuthConfig, ListenAddr, Server, ServerConfig, ToolContext, codec, forward::ForwardConfig,
};
use sandcrab_protocol::{
    AUTH_TOKEN_BYTES, AuthToken, ForwardAcceptParams, ForwardIncoming, ForwardListenParams,
    ForwardPortStats, ForwardRejectParams, ForwardStatsResult, RECORD_HEADER_LEN, RecordChannel,
    RecordKind, RpcMessage, RpcRequest, RpcResponse, codes, encode_record, parse_record_header,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UnixStream},
    sync::oneshot,
    task::JoinHandle,
};

const T: Duration = Duration::from_secs(5);

struct TestServer {
    _dir: TempDir,
    sock: PathBuf,
    ports: Vec<u16>,
    stop: Option<oneshot::Sender<()>>,
    handle: JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    async fn start(nports: usize, tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("crabd.sock");
        let mut ports = Vec::new();
        for _ in 0..nports {
            ports.push(free_port().await);
        }
        let mut config = ServerConfig::new(ListenAddr::Unix(sock.clone()));
        config.shutdown_grace = Duration::from_millis(500);
        config.forwards.ports = ports.clone();
        tweak(&mut config);
        let server = Server::bind(config, Arc::new(ToolContext::new(dir.path())))
            .await
            .unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(server.run(async {
            let _ = rx.await;
        }));
        Self {
            _dir: dir,
            sock,
            ports,
            stop: Some(tx),
            handle,
        }
    }

    fn port(&self) -> u16 {
        self.ports[0]
    }

    async fn connect(&self) -> UnixStream {
        UnixStream::connect(&self.sock).await.unwrap()
    }

    async fn listen(&self) -> UnixStream {
        let (s, resp) = self.try_listen().await;
        assert_eq!(result(&resp)["ports"], json!(self.ports), "{resp:?}");
        s
    }

    async fn try_listen(&self) -> (UnixStream, RpcResponse) {
        let mut s = self.connect().await;
        codec::write_frame(&mut s, &ForwardListenParams {}.to_request(1))
            .await
            .unwrap();
        let resp = recv(&mut s).await;
        (s, resp)
    }

    async fn accept(&self, conn_id: u64) -> (UnixStream, RpcResponse) {
        let mut s = self.connect().await;
        codec::write_frame(&mut s, &ForwardAcceptParams { conn_id }.to_request(1))
            .await
            .unwrap();
        let resp = recv(&mut s).await;
        (s, resp)
    }

    async fn call(&self, method: &str, params: Value) -> RpcResponse {
        let mut s = self.connect().await;
        codec::write_frame(&mut s, &RpcRequest::new(1, method, Some(params)))
            .await
            .unwrap();
        recv(&mut s).await
    }

    async fn stats(&self) -> Vec<ForwardPortStats> {
        let resp = self.call("forward.stats", json!({})).await;
        serde_json::from_value::<ForwardStatsResult>(result(&resp).clone())
            .unwrap()
            .0
    }

    async fn guest_connect(&self) -> TcpStream {
        TcpStream::connect(("127.0.0.1", self.port()))
            .await
            .unwrap()
    }

    async fn shutdown(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        tokio::time::timeout(T, self.handle)
            .await
            .expect("server stops")
            .unwrap()
            .unwrap();
    }
}

async fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

async fn recv<S: AsyncRead + Unpin>(s: &mut S) -> RpcResponse {
    let payload = tokio::time::timeout(T, codec::read_frame(s))
        .await
        .expect("frame in time")
        .unwrap()
        .expect("a frame, not EOF");
    serde_json::from_slice(&payload).unwrap()
}

async fn incoming(listen: &mut UnixStream) -> ForwardIncoming {
    let payload = tokio::time::timeout(T, codec::read_frame(listen))
        .await
        .expect("announce in time")
        .unwrap()
        .expect("a frame, not EOF");
    let RpcMessage::Notification(n) = RpcMessage::from_slice(&payload).unwrap() else {
        panic!("expected a notification")
    };
    ForwardIncoming::from_notification(&n).unwrap()
}

fn error_code(resp: &RpcResponse) -> i64 {
    match &resp.payload {
        sandcrab_protocol::ResponsePayload::Error(e) => e.code,
        sandcrab_protocol::ResponsePayload::Result(r) => panic!("expected error, got {r}"),
    }
}

fn result(resp: &RpcResponse) -> &Value {
    match &resp.payload {
        sandcrab_protocol::ResponsePayload::Result(r) => r,
        sandcrab_protocol::ResponsePayload::Error(e) => panic!("expected result, got {e:?}"),
    }
}

async fn send_data<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) {
    for chunk in bytes.chunks(64 * 1024) {
        w.write_all(&encode_record(RecordKind::Data, chunk).unwrap())
            .await
            .unwrap();
    }
}

async fn send_eof<W: AsyncWrite + Unpin>(w: &mut W) {
    w.write_all(&encode_record(RecordKind::Eof, &[]).unwrap())
        .await
        .unwrap();
}

/// Read DATA records until EOF; returns the bytes.
async fn read_until_eof<R: AsyncRead + Unpin>(r: &mut R) -> Vec<u8> {
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut h = [0u8; RECORD_HEADER_LEN];
            r.read_exact(&mut h).await.unwrap();
            let (kind, len) = parse_record_header(RecordChannel::Forward, h).unwrap();
            if kind == RecordKind::Eof {
                return;
            }
            let start = out.len();
            out.resize(start + len, 0);
            r.read_exact(&mut out[start..]).await.unwrap();
        }
    })
    .await
    .expect("EOF record in time");
    out
}

/// The guest side was closed by `crabd`: EOF or reset.
async fn assert_closed<S: AsyncRead + Unpin>(s: &mut S) {
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(T, s.read(&mut buf))
        .await
        .expect("closed in time")
        .unwrap_or(0);
    assert_eq!(n, 0, "expected the connection to be closed");
}

fn mixed(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[tokio::test]
async fn hello_reports_forward_ports_and_feature() {
    let srv = TestServer::start(2, |_| {}).await;
    let resp = srv.call("hello", json!({})).await;
    let r = result(&resp);
    assert_eq!(r["forward_ports"], json!(srv.ports));
    let features: Vec<String> = serde_json::from_value(r["features"].clone()).unwrap();
    assert!(features.contains(&"forward".to_owned()), "{features:?}");
    let stats = srv.stats().await;
    assert_eq!(stats.len(), 2);
    assert_eq!(stats[0].port, srv.ports[0]);
    assert_eq!(stats[1].port, srv.ports[1]);
    srv.shutdown().await;
}

#[tokio::test]
async fn listeners_are_bound_at_bind_and_loopback_only() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port().await;
    let mut config = ServerConfig::new(ListenAddr::Unix(dir.path().join("s.sock")));
    config.forwards.ports = vec![port];
    let server = Server::bind(config, Arc::new(ToolContext::new(dir.path())))
        .await
        .unwrap();
    // Not running yet, yet the kernel already accepts: the listener exists before `hello`.
    TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    #[cfg(target_os = "linux")]
    {
        let hex_port = format!(":{port:04X}");
        let mut seen = 0;
        for (file, loopback) in [
            ("/proc/net/tcp", "0100007F"),
            ("/proc/net/tcp6", "00000000000000000000000001000000"),
        ] {
            let table = std::fs::read_to_string(file).unwrap_or_default();
            for line in table.lines().skip(1) {
                let cols: Vec<&str> = line.split_whitespace().collect();
                // Column 1 is the local address, column 3 the state (0A = LISTEN).
                if cols.len() > 3 && cols[1].ends_with(&hex_port) && cols[3] == "0A" {
                    assert_eq!(cols[1], format!("{loopback}{hex_port}"), "{file}: {line}");
                    seen += 1;
                }
            }
        }
        assert!(seen >= 1, "no LISTEN entry for port {port}");
    }
    // A non-loopback address of this host does not reach the forward.
    if let Some(ip) = outward_ip() {
        let r = tokio::time::timeout(T, TcpStream::connect(SocketAddr::new(ip, port))).await;
        assert!(
            !matches!(r, Ok(Ok(_))),
            "forward port reachable on {ip}: not loopback only"
        );
    }
    drop(server);
}

/// The address this host would use towards the outside (no packet is sent).
fn outward_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[tokio::test]
async fn a_port_not_in_the_list_is_refused_by_the_kernel() {
    let srv = TestServer::start(1, |_| {}).await;
    let _listen = srv.listen().await;
    let other = free_port().await;
    let err = TcpStream::connect(("127.0.0.1", other)).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
    srv.shutdown().await;
}

#[tokio::test]
async fn bad_port_lists_fail_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let p = free_port().await;
    let held = TcpListener::bind(("127.0.0.1", p)).await.unwrap();
    for ports in [vec![p], vec![0], vec![1000, 1000], (2000..2017).collect()] {
        let mut config = ServerConfig::new(ListenAddr::Unix(dir.path().join("s.sock")));
        config.forwards.ports = ports.clone();
        assert!(
            Server::bind(config, Arc::new(ToolContext::new(dir.path())))
                .await
                .is_err(),
            "{ports:?}"
        );
    }
    drop(held);
}

/// 1 MiB guest → host, guest half-closes, then 1 MiB host → guest: both arrive identical.
#[tokio::test]
async fn relays_1mib_both_ways_with_guest_half_close() {
    let srv = TestServer::start(1, |_| {}).await;
    let mut listen = srv.listen().await;
    let up = mixed(1 << 20, 7);
    let down = mixed(1 << 20, 99);

    let guest = {
        let mut g = srv.guest_connect().await;
        let up = up.clone();
        tokio::spawn(async move {
            g.write_all(&up).await.unwrap();
            g.shutdown().await.unwrap(); // upload until EOF
            let mut got = Vec::new();
            g.read_to_end(&mut got).await.unwrap();
            got
        })
    };
    let inc = incoming(&mut listen).await;
    assert_eq!(inc.port, srv.port());
    let (mut host, resp) = srv.accept(inc.conn_id).await;
    assert_eq!(result(&resp), &json!({"port": srv.port()}));
    assert_eq!(read_until_eof(&mut host).await, up);
    send_data(&mut host, &down).await;
    send_eof(&mut host).await;
    let got = tokio::time::timeout(Duration::from_secs(20), guest)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, down);
    // Both directions ended: crabd closes the host connection.
    assert_closed(&mut host).await;

    let s = srv.stats().await;
    assert_eq!(s[0].accepted, 1);
    // A conn_id is used once.
    let (_s, again) = srv.accept(inc.conn_id).await;
    assert_eq!(error_code(&again), codes::NOT_FOUND);
    srv.shutdown().await;
}

/// The host ends first (EOF record); the guest still sends afterwards.
#[tokio::test]
async fn host_half_close_keeps_the_guest_direction_open() {
    let srv = TestServer::start(1, |_| {}).await;
    let mut listen = srv.listen().await;
    let guest = {
        let mut g = srv.guest_connect().await;
        tokio::spawn(async move {
            let mut got = Vec::new();
            g.read_to_end(&mut got).await.unwrap(); // the host's EOF arrives as FIN
            assert_eq!(got, b"request");
            g.write_all(b"reply after eof").await.unwrap();
            g.shutdown().await.unwrap();
        })
    };
    let inc = incoming(&mut listen).await;
    let (mut host, _) = srv.accept(inc.conn_id).await;
    send_data(&mut host, b"request").await;
    send_eof(&mut host).await;
    assert_eq!(read_until_eof(&mut host).await, b"reply after eof");
    guest.await.unwrap();
    assert_closed(&mut host).await;
    srv.shutdown().await;
}

#[tokio::test]
async fn no_listen_connection_closes_and_counts() {
    let srv = TestServer::start(1, |_| {}).await;
    let mut g = srv.guest_connect().await;
    assert_closed(&mut g).await;
    // A listen connection that went away counts the same.
    drop(srv.listen().await);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut g = srv.guest_connect().await;
    assert_closed(&mut g).await;
    let s = srv.stats().await;
    assert_eq!(s[0].refused.no_listener, 2, "{s:?}");
    assert_eq!(s[0].accepted, 0);
    srv.shutdown().await;
}

#[tokio::test]
async fn reject_closes_and_counts() {
    let srv = TestServer::start(1, |_| {}).await;
    let mut listen = srv.listen().await;
    let mut g = srv.guest_connect().await;
    let inc = incoming(&mut listen).await;
    codec::write_frame(
        &mut listen,
        &ForwardRejectParams {
            conn_id: inc.conn_id,
        }
        .to_notification(),
    )
    .await
    .unwrap();
    assert_closed(&mut g).await;
    assert_eq!(srv.stats().await[0].refused.rejected, 1);
    let (_s, resp) = srv.accept(inc.conn_id).await;
    assert_eq!(error_code(&resp), codes::NOT_FOUND);
    // An unknown id is ignored; the listen connection keeps working.
    codec::write_frame(
        &mut listen,
        &ForwardRejectParams { conn_id: 999_999 }.to_notification(),
    )
    .await
    .unwrap();
    let _g2 = srv.guest_connect().await;
    incoming(&mut listen).await;
    srv.shutdown().await;
}

#[tokio::test]
async fn unclaimed_connection_is_closed_at_the_deadline() {
    let srv = TestServer::start(1, |c| {
        c.forwards.unclaimed_timeout = Duration::from_millis(300);
    })
    .await;
    let mut listen = srv.listen().await;
    let mut g = srv.guest_connect().await;
    let inc = incoming(&mut listen).await;
    assert_closed(&mut g).await;
    assert_eq!(srv.stats().await[0].refused.unclaimed, 1);
    let (_s, resp) = srv.accept(inc.conn_id).await;
    assert_eq!(error_code(&resp), codes::NOT_FOUND);
    srv.shutdown().await;
}

#[tokio::test]
async fn default_deadline_is_ten_seconds() {
    assert_eq!(
        ForwardConfig::default().unclaimed_timeout,
        Duration::from_secs(10)
    );
}

#[tokio::test]
async fn caps_close_at_once_and_count() {
    // Per forward.
    let srv = TestServer::start(1, |c| c.forwards.max_conns_per_forward = 2).await;
    let mut listen = srv.listen().await;
    let a = srv.guest_connect().await;
    let _b = srv.guest_connect().await;
    let mut c = srv.guest_connect().await;
    let (ia, ib) = (incoming(&mut listen).await, incoming(&mut listen).await);
    assert_closed(&mut c).await;
    assert_eq!(srv.stats().await[0].refused.cap, 1);
    // An accepted connection still holds its slot; a finished one gives it back.
    let (host, _) = srv.accept(ia.conn_id).await;
    let mut d = srv.guest_connect().await;
    assert_closed(&mut d).await;
    assert_eq!(srv.stats().await[0].refused.cap, 2);
    drop(host);
    drop(a);
    codec::write_frame(
        &mut listen,
        &ForwardRejectParams {
            conn_id: ib.conn_id,
        }
        .to_notification(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _e = srv.guest_connect().await;
    incoming(&mut listen).await;
    srv.shutdown().await;

    // Per sandbox, across forwards.
    let srv = TestServer::start(2, |c| c.forwards.max_conns = 1).await;
    let mut listen = srv.listen().await;
    let _a = srv.guest_connect().await;
    incoming(&mut listen).await;
    let mut b = TcpStream::connect(("127.0.0.1", srv.ports[1]))
        .await
        .unwrap();
    assert_closed(&mut b).await;
    let s = srv.stats().await;
    assert_eq!((s[0].refused.cap, s[1].refused.cap), (0, 1), "{s:?}");
    srv.shutdown().await;

    // Announced and not yet accepted.
    let srv = TestServer::start(1, |c| c.forwards.max_pending = 1).await;
    let mut listen = srv.listen().await;
    let _a = srv.guest_connect().await;
    let ia = incoming(&mut listen).await;
    let mut b = srv.guest_connect().await;
    assert_closed(&mut b).await;
    assert_eq!(srv.stats().await[0].refused.cap, 1);
    // Accepting frees the pending place (not the connection slot).
    let (_host, _) = srv.accept(ia.conn_id).await;
    let _c = srv.guest_connect().await;
    incoming(&mut listen).await;
    srv.shutdown().await;
}

#[tokio::test]
async fn second_listen_is_refused_while_the_first_lives() {
    let srv = TestServer::start(1, |_| {}).await;
    let mut first = srv.listen().await;
    let (mut second, resp) = srv.try_listen().await;
    assert_eq!(error_code(&resp), codes::EXECUTION_FAILED);
    assert!(matches!(
        codec::read_frame(&mut second).await,
        Ok(None) | Err(_)
    ));
    // The first one still gets announcements.
    let _g = srv.guest_connect().await;
    incoming(&mut first).await;
    // Once it is gone, a new listen is accepted.
    drop(first);
    let mut ok = false;
    for _ in 0..50 {
        let (_s, resp) = srv.try_listen().await;
        if matches!(resp.payload, sandcrab_protocol::ResponsePayload::Result(_)) {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ok, "a new forward.listen after the first closed");
    srv.shutdown().await;
}

#[tokio::test]
async fn host_drop_closes_the_guest_connection() {
    let srv = TestServer::start(1, |_| {}).await;
    let mut listen = srv.listen().await;
    let mut g = srv.guest_connect().await;
    let inc = incoming(&mut listen).await;
    let (mut host, _) = srv.accept(inc.conn_id).await;
    send_data(&mut host, b"partial").await;
    drop(host); // no EOF record
    let mut got = Vec::new();
    tokio::time::timeout(T, g.read_to_end(&mut got))
        .await
        .unwrap()
        .ok();
    assert_eq!(got, b"partial");
    srv.shutdown().await;
}

#[tokio::test]
async fn a_bad_record_closes_both_sides() {
    let srv = TestServer::start(1, |_| {}).await;
    let mut listen = srv.listen().await;
    let mut g = srv.guest_connect().await;
    let inc = incoming(&mut listen).await;
    let (mut host, _) = srv.accept(inc.conn_id).await;
    // A process kind is not valid on a forward connection.
    host.write_all(&encode_record(RecordKind::Stdin, b"x").unwrap())
        .await
        .unwrap();
    assert_closed(&mut g).await;
    srv.shutdown().await;
}

#[tokio::test]
async fn forward_verbs_only_as_first_frame() {
    let srv = TestServer::start(1, |_| {}).await;
    let mut s = srv.connect().await;
    codec::write_frame(&mut s, &RpcRequest::new(1, "ping", None))
        .await
        .unwrap();
    recv(&mut s).await;
    for req in [
        ForwardListenParams {}.to_request(2),
        ForwardAcceptParams { conn_id: 1 }.to_request(3),
    ] {
        codec::write_frame(&mut s, &req).await.unwrap();
        assert_eq!(error_code(&recv(&mut s).await), codes::INVALID_REQUEST);
    }
    // Bad params on the verb itself.
    let resp = srv.call("forward.accept", json!({"conn_id": "x"})).await;
    assert_eq!(error_code(&resp), codes::INVALID_PARAMS);
    srv.shutdown().await;
}

/// With a token, `forward.listen` / `forward.accept` without `auth` first are refused: nothing
/// is registered and nothing is handed over.
#[tokio::test]
async fn forwards_need_auth_first() {
    let token = AuthToken::from_bytes(&[0x42; AUTH_TOKEN_BYTES]);
    let srv = TestServer::start(1, |c| {
        c.auth = Some(AuthConfig {
            token: token.clone(),
            timeout: Duration::from_millis(300),
        });
    })
    .await;
    let (mut s, resp) = srv.try_listen().await;
    assert_eq!(error_code(&resp), codes::UNAUTHORIZED);
    assert!(matches!(codec::read_frame(&mut s).await, Ok(None) | Err(_)));
    // No listen connection was registered: the guest connection is refused as such.
    let mut g = srv.guest_connect().await;
    assert_closed(&mut g).await;

    // With auth, the same flow works.
    let mut listen = srv.connect().await;
    codec::write_frame(&mut listen, &token.to_request(0))
        .await
        .unwrap();
    recv(&mut listen).await;
    codec::write_frame(&mut listen, &ForwardListenParams {}.to_request(1))
        .await
        .unwrap();
    result(&recv(&mut listen).await);
    let _g = srv.guest_connect().await;
    let inc = incoming(&mut listen).await;
    // forward.accept without auth is refused and does not consume the id.
    let (_s, resp) = srv.accept(inc.conn_id).await;
    assert_eq!(error_code(&resp), codes::UNAUTHORIZED);
    let mut acc = srv.connect().await;
    codec::write_frame(&mut acc, &token.to_request(0))
        .await
        .unwrap();
    recv(&mut acc).await;
    codec::write_frame(
        &mut acc,
        &ForwardAcceptParams {
            conn_id: inc.conn_id,
        }
        .to_request(1),
    )
    .await
    .unwrap();
    assert_eq!(result(&recv(&mut acc).await), &json!({"port": srv.port()}));
    srv.shutdown().await;
}

/// The core cases inside a hardened Docker container with network `none`: the container's own
/// loopback carries the forward listener. Needs Docker and a static musl `crabd`
/// (`SANDCRAB_CRABD`, or `scripts/build-crabd.sh` output under the target dir). Run under the
/// e2e lock.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "needs Docker and a static crabd"]
async fn docker_network_none_forward() {
    use std::process::Command;

    const IMAGE: &str = "debian:trixie-20261104-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2";
    const PORT: u16 = 15432;

    let crabd = std::env::var_os("SANDCRAB_CRABD")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
                || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"),
                PathBuf::from,
            );
            target.join(format!(
                "{}-unknown-linux-musl/release/crabd",
                std::env::consts::ARCH
            ))
        });
    assert!(
        crabd.is_file(),
        "no static crabd at {} (set SANDCRAB_CRABD or run scripts/build-crabd.sh)",
        crabd.display()
    );

    struct Container(String);
    impl Drop for Container {
        fn drop(&mut self) {
            let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let sockdir = dir.path().join("s");
    std::fs::create_dir(&sockdir).unwrap();
    let user = format!(
        "{}:{}",
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw()
    );
    let name = format!("sandcrab-fwd-test-{}", std::process::id());
    let out = Command::new("docker")
        .args(["run", "-d", "--name", &name, "--network", "none"])
        .args([
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges:true",
        ])
        .args(["--read-only", "--tmpfs", "/tmp", "--pids-limit", "256"])
        .args(["--user", &user])
        .arg("-v")
        .arg(format!("{}:/opt/sandcrab/crabd:ro", crabd.display()))
        .arg("-v")
        .arg(format!("{}:/s", sockdir.display()))
        .args([
            IMAGE,
            "/opt/sandcrab/crabd",
            "serve",
            "--unix",
            "/s/crabd.sock",
        ])
        .args(["--default-cwd", "/tmp", "--forward-port", &PORT.to_string()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "docker run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _container = Container(name.clone());

    let sock = sockdir.join("crabd.sock");
    let connect = || d_connect(sock.clone());
    let call = |method: &'static str, params: Value| d_call(sock.clone(), method, params);
    let bash = |cmd: String| d_bash(sock.clone(), cmd);

    let hello = call("hello", json!({})).await;
    assert_eq!(result(&hello)["forward_ports"], json!([PORT]));

    // Loopback only, inside the container.
    let tables = bash("cat /proc/net/tcp /proc/net/tcp6 2>/dev/null".into()).await;
    let hex_port = format!(":{PORT:04X}");
    let mut seen = 0;
    for line in tables["stdout"].as_str().unwrap().lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() > 3 && cols[1].ends_with(&hex_port) && cols[3] == "0A" {
            assert!(
                cols[1].starts_with("0100007F:")
                    || cols[1].starts_with("00000000000000000000000001000000:"),
                "{line}"
            );
            seen += 1;
        }
    }
    assert!(seen >= 1, "no listener for {PORT}: {tables}");

    // A port that is not forwarded: refused.
    let r = bash("exec 3<>/dev/tcp/127.0.0.1/15433".into()).await;
    assert_ne!(r["exit_code"], json!(0), "{r}");

    // No listen connection: closed at once.
    let client = format!("exec 3<>/dev/tcp/127.0.0.1/{PORT} && cat <&3; echo done");
    let r = bash(client.clone()).await;
    assert_eq!(r["stdout"], json!("done\n"), "{r}");

    // Listen, accept, relay both ways.
    let mut listen = connect().await;
    codec::write_frame(&mut listen, &ForwardListenParams {}.to_request(1))
        .await
        .unwrap();
    recv(&mut listen).await;
    let guest = tokio::spawn(bash(format!(
        "exec 3<>/dev/tcp/127.0.0.1/{PORT} && printf ping >&3 && head -c 4 <&3"
    )));
    let inc = incoming(&mut listen).await;
    let mut host = connect().await;
    codec::write_frame(
        &mut host,
        &ForwardAcceptParams {
            conn_id: inc.conn_id,
        }
        .to_request(1),
    )
    .await
    .unwrap();
    assert_eq!(result(&recv(&mut host).await), &json!({"port": PORT}));
    let mut got = Vec::new();
    while got.len() < 4 {
        let mut h = [0u8; RECORD_HEADER_LEN];
        host.read_exact(&mut h).await.unwrap();
        let (kind, len) = parse_record_header(RecordChannel::Forward, h).unwrap();
        assert_eq!(kind, RecordKind::Data);
        let start = got.len();
        got.resize(start + len, 0);
        host.read_exact(&mut got[start..]).await.unwrap();
    }
    assert_eq!(got, b"ping");
    send_data(&mut host, b"pong").await;
    send_eof(&mut host).await;
    let r = guest.await.unwrap();
    assert_eq!(r["stdout"], json!("pong"), "{r}");

    // Reject.
    let guest = tokio::spawn(bash(client));
    let inc = incoming(&mut listen).await;
    codec::write_frame(
        &mut listen,
        &ForwardRejectParams {
            conn_id: inc.conn_id,
        }
        .to_notification(),
    )
    .await
    .unwrap();
    assert_eq!(guest.await.unwrap()["stdout"], json!("done\n"));

    let stats = call("forward.stats", json!({})).await;
    let s = serde_json::from_value::<ForwardStatsResult>(result(&stats).clone())
        .unwrap()
        .0;
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].accepted, 1, "{s:?}");
    assert_eq!(s[0].refused.no_listener, 1, "{s:?}");
    assert_eq!(s[0].refused.rejected, 1, "{s:?}");
}

#[cfg(target_os = "linux")]
async fn d_connect(sock: PathBuf) -> UnixStream {
    for _ in 0..200 {
        if let Ok(s) = UnixStream::connect(&sock).await {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("crabd did not come up at {}", sock.display());
}

#[cfg(target_os = "linux")]
async fn d_call(sock: PathBuf, method: &str, params: Value) -> RpcResponse {
    let mut s = d_connect(sock).await;
    codec::write_frame(&mut s, &RpcRequest::new(1, method, Some(params)))
        .await
        .unwrap();
    let payload = tokio::time::timeout(Duration::from_secs(30), codec::read_frame(&mut s))
        .await
        .expect("answer in time")
        .unwrap()
        .unwrap();
    serde_json::from_slice(&payload).unwrap()
}

#[cfg(target_os = "linux")]
async fn d_bash(sock: PathBuf, cmd: String) -> Value {
    let resp = d_call(sock, "bash", json!({"command": cmd, "timeout_ms": 20_000})).await;
    result(&resp).clone()
}
