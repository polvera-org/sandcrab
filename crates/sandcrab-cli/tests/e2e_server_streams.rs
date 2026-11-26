//! `sandcrab server` on Docker: real processes and real forwards through the server, driven
//! by a raw client that plays the SDK (it serves the forwards itself: notification, connect
//! the target, `forward.attach`, pipe).
//!
//! Prints the warm `/bin/true` spawn → exit times and the forwarded throughput. Needs what
//! `e2e_server.rs` needs (Docker, the pinned `base` template, a static `crabd`). Run with
//! `cargo test -p sandcrab-cli --test e2e_server_streams -- --ignored --nocapture`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};

const ECHO_PORT: u16 = 15001;
const SINK_PORT: u16 = 15002;
const SOURCE_PORT: u16 = 15003;
const STREAM_BYTES: u64 = 256 * 1024 * 1024;
const PATH: &str = "/usr/local/bin:/usr/bin:/bin";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

async fn send_frame(s: &mut UnixStream, v: &Value) {
    let body = serde_json::to_vec(v).unwrap();
    s.write_all(&u32::try_from(body.len()).unwrap().to_be_bytes())
        .await
        .unwrap();
    s.write_all(&body).await.unwrap();
}

async fn read_frame(s: &mut UnixStream) -> Option<Value> {
    let mut h = [0u8; 4];
    s.read_exact(&mut h).await.ok()?;
    let mut body = vec![0u8; u32::from_be_bytes(h) as usize];
    s.read_exact(&mut body).await.ok()?;
    Some(serde_json::from_slice(&body).unwrap())
}

struct Control {
    s: UnixStream,
    next: i64,
}

impl Control {
    async fn connect(path: &Path) -> Self {
        let mut c = Self {
            s: UnixStream::connect(path).await.unwrap(),
            next: 1,
        };
        let hello = c
            .call(
                "server.hello",
                json!({"client": "e2e", "api_version": "1.0"}),
            )
            .await;
        assert_eq!(
            hello["features"],
            json!(["forwards", "mounts", "selftest"]),
            "{hello}"
        );
        c
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        send_frame(
            &mut self.s,
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .await;
        loop {
            let r = read_frame(&mut self.s).await.expect("an answer");
            if r.get("id").is_none() {
                continue; // a notification
            }
            return r
                .get("result")
                .cloned()
                .unwrap_or_else(|| panic!("{method}: {r}"));
        }
    }
}

/// A `sandcrab server` child that cannot outlive the test: it runs with `--lifeline-stdin`
/// and this process holds the pipe (closed when the test process dies, also on a kill), and
/// dropping the guard (also on a panic) kills and reaps it.
struct ServerChild(Child);

impl Drop for ServerChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start(state: &Path, socket: &Path) -> ServerChild {
    let child = Command::new(env!("CARGO_BIN_EXE_sandcrab"))
        .arg("--backend=docker")
        .arg("--state-dir")
        .arg(state)
        .arg("--templates-dir")
        .arg(repo_root().join("templates"))
        .arg("server")
        .arg("--socket")
        .arg(socket)
        .arg("--lifeline-stdin")
        .stdin(Stdio::piped())
        .spawn()
        .expect("start sandcrab server");
    let child = ServerChild(child);
    let deadline = Instant::now() + Duration::from_secs(60);
    while UnixStream::connect(socket).await.is_err() {
        assert!(Instant::now() < deadline, "the server did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    child
}

/// What a process connection delivered.
#[derive(Debug, Default)]
struct Ran {
    answer: Value,
    stdout: Vec<u8>,
    exit: Option<Value>,
}

impl Ran {
    fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// A fresh UUID-shaped process id (unique within this test's server).
fn process_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        "00000000-0000-4000-8000-{:012x}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

async fn open(socket: &Path, sandbox: &str, argv: &[&str], extra: Value) -> (UnixStream, Value) {
    let mut s = UnixStream::connect(socket).await.unwrap();
    let mut params = json!({
        "process_id": process_id(),
        "sandbox_id": sandbox,
        "argv": argv,
        "env": {"PATH": PATH},
    });
    for (k, v) in extra.as_object().unwrap() {
        params[k] = v.clone();
    }
    send_frame(
        &mut s,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "process.spawn", "params": params}),
    )
    .await;
    let answer = read_frame(&mut s)
        .await
        .expect("an answer to process.spawn");
    (s, answer)
}

async fn collect<R: AsyncReadExt + Unpin>(rd: &mut R) -> (Vec<u8>, Option<Value>) {
    let mut out = Vec::new();
    loop {
        let mut h = [0u8; 5];
        if rd.read_exact(&mut h).await.is_err() {
            return (out, None);
        }
        let mut p = vec![0u8; u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize];
        rd.read_exact(&mut p).await.unwrap();
        match h[0] {
            0x11 | 0x12 => out.extend_from_slice(&p),
            0x1F => return (out, Some(serde_json::from_slice(&p).unwrap())),
            k => panic!("record {k:#x}"),
        }
    }
}

async fn run(socket: &Path, sandbox: &str, argv: &[&str], extra: Value) -> Ran {
    let (mut s, answer) = open(socket, sandbox, argv, extra).await;
    if answer.get("error").is_some() {
        return Ran {
            answer,
            ..Ran::default()
        };
    }
    let (stdout, exit) = collect(&mut s).await;
    Ran {
        answer,
        stdout,
        exit,
    }
}

async fn bash(socket: &Path, sandbox: &str, script: &str) -> Ran {
    let r = tokio::time::timeout(
        Duration::from_secs(300),
        run(
            socket,
            sandbox,
            &["/bin/bash", "-c", script],
            json!({"timeout_ms": 290_000}),
        ),
    )
    .await
    .expect("the script ended");
    assert!(r.answer.get("result").is_some(), "{r:?}");
    r
}

/// The host targets of the forwards: loopback listeners of this test process.
struct Targets {
    addrs: HashMap<u16, SocketAddr>,
    echo_open: AtomicUsize,
    echo_peak: AtomicUsize,
    /// Bytes and nanoseconds of the last sink stream (first byte to end-of-file).
    sunk: (AtomicU64, AtomicU64),
}

async fn targets() -> Arc<Targets> {
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sink = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let t = Arc::new(Targets {
        addrs: HashMap::from([
            (ECHO_PORT, echo.local_addr().unwrap()),
            (SINK_PORT, sink.local_addr().unwrap()),
            (SOURCE_PORT, source.local_addr().unwrap()),
        ]),
        echo_open: AtomicUsize::new(0),
        echo_peak: AtomicUsize::new(0),
        sunk: (AtomicU64::new(0), AtomicU64::new(0)),
    });
    let te = Arc::clone(&t);
    tokio::spawn(async move {
        while let Ok((s, _)) = echo.accept().await {
            let t = Arc::clone(&te);
            tokio::spawn(async move {
                let n = t.echo_open.fetch_add(1, Ordering::SeqCst) + 1;
                t.echo_peak.fetch_max(n, Ordering::SeqCst);
                let (mut r, mut w) = s.into_split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
                let _ = w.shutdown().await;
                t.echo_open.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    let ts = Arc::clone(&t);
    tokio::spawn(async move {
        while let Ok((mut s, _)) = sink.accept().await {
            let t = Arc::clone(&ts);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 1 << 16];
                let mut n = 0u64;
                let mut first = None;
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(k) => {
                            first.get_or_insert_with(Instant::now);
                            n += k as u64;
                        }
                    }
                }
                let ns = first.map_or(0, |f| f.elapsed().as_nanos());
                t.sunk.1.store(u64::try_from(ns).unwrap(), Ordering::SeqCst);
                t.sunk.0.store(n, Ordering::SeqCst);
            });
        }
    });
    tokio::spawn(async move {
        while let Ok((mut s, _)) = source.accept().await {
            tokio::spawn(async move {
                let buf = vec![0u8; 1 << 16];
                let mut left = STREAM_BYTES;
                while left > 0 {
                    let k = left.min(buf.len() as u64) as usize;
                    if s.write_all(&buf[..k]).await.is_err() {
                        return;
                    }
                    left -= k as u64;
                }
                let _ = s.shutdown().await;
            });
        }
    });
    t
}

/// Serve the forwards announced on `control` (the connection that created the sandbox):
/// connect the target first, then attach, then pipe. A target that fails is rejected.
fn serve_forwards(mut control: Control, socket: PathBuf, targets: Arc<Targets>) {
    tokio::spawn(async move {
        while let Some(m) = read_frame(&mut control.s).await {
            if m["method"] != "forward.incoming" {
                continue;
            }
            let conn_id = m["params"]["conn_id"].clone();
            let port = u16::try_from(m["params"]["guest_port"].as_u64().unwrap()).unwrap();
            let addr = targets.addrs[&port];
            let socket = socket.clone();
            tokio::spawn(async move {
                let mut target = TcpStream::connect(addr).await.unwrap();
                let mut a = UnixStream::connect(&socket).await.unwrap();
                send_frame(
                    &mut a,
                    &json!({"jsonrpc": "2.0", "id": 1, "method": "forward.attach",
                            "params": {"conn_id": conn_id}}),
                )
                .await;
                let ack = read_frame(&mut a).await.expect("an attach answer");
                assert_eq!(ack["result"], json!({}), "{ack}");
                let _ = tokio::io::copy_bidirectional(&mut target, &mut a).await;
            });
        }
    });
}

fn mb_per_s(bytes: u64, nanos: u64) -> f64 {
    bytes as f64 / 1e6 / (nanos as f64 / 1e9)
}

fn pct(sorted: &[Duration], p: usize) -> Duration {
    sorted[(sorted.len() * p / 100).min(sorted.len() - 1)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "e2e: needs Docker, the pinned base template and a static crabd"]
async fn server_on_docker_processes_and_forwards() {
    let tmp = tempfile::Builder::new()
        .prefix("sc-str")
        .tempdir_in("/tmp")
        .unwrap();
    let state = tmp.path().join("state");
    let socket = tmp.path().join("sock/server.sock");
    let mut server = start(&state, &socket).await;
    let targets = targets().await;

    // The creating connection serves the forwards; a second one makes the calls.
    let mut creator = Control::connect(&socket).await;
    let info = creator
        .call(
            "sandbox.create",
            json!({"template": "base", "labels": {"e2e": "streams"}, "forwards": [
                {"guest_port": ECHO_PORT}, {"guest_port": SINK_PORT}, {"guest_port": SOURCE_PORT}
            ]}),
        )
        .await;
    assert_eq!(info["forwards_served"], true, "{info}");
    let sb = info["sandbox_id"].as_str().unwrap().to_owned();
    serve_forwards(creator, socket.clone(), Arc::clone(&targets));
    let mut c = Control::connect(&socket).await;

    // A real process: stdout, the stderr tail, the exit code.
    let r = bash(&socket, &sb, "echo out; echo err >&2; exit 3").await;
    assert_eq!(r.out(), "out\n");
    let exit = r.exit.unwrap();
    assert_eq!(exit["code"], 3);
    assert_eq!(exit["stderr_tail"], "err\n");
    assert_eq!(exit["tree_clean"], true);
    // The exact environment.
    let r = run(&socket, &sb, &["/usr/bin/env"], json!({})).await;
    assert_eq!(r.out(), format!("PATH={PATH}\n"));
    // A program that cannot start.
    let r = run(&socket, &sb, &["/no/such"], json!({})).await;
    assert_eq!(r.answer["error"]["code"], -32010, "{r:?}");
    assert_eq!(r.answer["error"]["data"]["errno"], "ENOENT");

    // 1 MiB through `cat`, both ways at once.
    let (s, answer) = open(&socket, &sb, &["/bin/cat"], json!({"stdin": "pipe"})).await;
    assert!(answer.get("result").is_some(), "{answer}");
    let (mut rd, mut wr) = s.into_split();
    let data: Vec<u8> = (0..1 << 20).map(|i: u32| (i % 251) as u8).collect();
    let sent = data.clone();
    let writer = tokio::spawn(async move {
        for chunk in sent.chunks(65536) {
            let mut rec = vec![0x01];
            rec.extend_from_slice(&u32::try_from(chunk.len()).unwrap().to_be_bytes());
            rec.extend_from_slice(chunk);
            wr.write_all(&rec).await.unwrap();
        }
        wr.write_all(&[0x02, 0, 0, 0, 0]).await.unwrap();
        wr
    });
    let (out, exit) = collect(&mut rd).await;
    let _wr = writer.await.unwrap();
    assert!(out == data, "1 MiB through cat: {} bytes back", out.len());
    assert_eq!(exit.unwrap()["code"], 0);

    // Cancel kills a real process.
    let (s, answer) = open(&socket, &sb, &["/bin/sleep", "1000"], json!({})).await;
    let pid = answer["result"]["process_id"].clone();
    let collector = tokio::spawn(async move {
        let mut s = s;
        collect(&mut s).await
    });
    let r = c
        .call(
            "process.cancel",
            json!({"process_id": pid, "grace_ms": 500}),
        )
        .await;
    assert_eq!(r, json!({"was_running": true, "tree_clean": true}));
    let (_, exit) = collector.await.unwrap();
    assert_eq!(exit.unwrap()["cancelled"], true);

    // Warm spawn → exit of /bin/true, timed at this client: 20 warm-up, 200 sequential.
    let mut times = Vec::new();
    for i in 0..220 {
        let t = Instant::now();
        let r = run(&socket, &sb, &["/bin/true"], json!({})).await;
        let d = t.elapsed();
        assert_eq!(r.exit.as_ref().unwrap()["code"], 0, "{r:?}");
        if i >= 20 {
            times.push(d);
        }
    }
    times.sort();
    println!(
        "server warm /bin/true spawn->exit, 200 sequential: p50 {:?}, p95 {:?}, max {:?}",
        pct(&times, 50),
        pct(&times, 95),
        times[times.len() - 1]
    );

    // One forwarded connection, both ways.
    let r = bash(
        &socket,
        &sb,
        &format!(
            "exec 3<>/dev/tcp/127.0.0.1/{ECHO_PORT} || exit 9; echo hello >&3; \
             read -r -t 20 l <&3; echo \"got $l\""
        ),
    )
    .await;
    assert_eq!(r.out(), "got hello\n", "{r:?}");

    // 16 at once, all open together, none reset.
    let r = bash(
        &socket,
        &sb,
        &format!(
            "for i in $(seq 1 16); do ( exec 3<>/dev/tcp/127.0.0.1/{ECHO_PORT} || exit 9; \
             echo \"c$i\" >&3; sleep 1; read -r -t 20 l <&3 || echo \"lost$i\"; echo \"$l\" ) & \
             done; wait"
        ),
    )
    .await;
    let mut got: Vec<_> = r.out().lines().map(str::to_owned).collect();
    got.sort();
    let mut want: Vec<_> = (1..=16).map(|i| format!("c{i}")).collect();
    want.sort();
    assert_eq!(got, want, "{r:?}");
    assert_eq!(targets.echo_peak.load(Ordering::SeqCst), 16);

    // Throughput of one stream, sandbox → client target: upload until end-of-file.
    let r = bash(
        &socket,
        &sb,
        &format!("head -c {STREAM_BYTES} /dev/zero > /dev/tcp/127.0.0.1/{SINK_PORT}"),
    )
    .await;
    assert_eq!(r.exit.as_ref().unwrap()["code"], 0, "{r:?}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while targets.sunk.0.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the sink never saw end-of-file");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(targets.sunk.0.load(Ordering::SeqCst), STREAM_BYTES);
    let up = mb_per_s(STREAM_BYTES, targets.sunk.1.load(Ordering::SeqCst));
    // Client target → sandbox, timed in the guest.
    let r = bash(
        &socket,
        &sb,
        &format!(
            "t0=$(date +%s%N); exec 3<>/dev/tcp/127.0.0.1/{SOURCE_PORT} || exit 9; \
             n=$(cat <&3 | wc -c); echo \"$n $(( $(date +%s%N) - t0 ))\""
        ),
    )
    .await;
    let nums: Vec<u64> = r
        .out()
        .split_whitespace()
        .map(|n| n.parse().unwrap())
        .collect();
    assert_eq!(nums[0], STREAM_BYTES, "{r:?}");
    let down = mb_per_s(STREAM_BYTES, nums[1]);
    println!(
        "server forward throughput, one stream of {} MiB end to end: sandbox->client {up:.0} MB/s, \
         client->sandbox {down:.0} MB/s (target 50 MB/s)",
        STREAM_BYTES / (1024 * 1024)
    );

    // Counters: everything accepted, nothing refused.
    let info = c.call("sandbox.get", json!({"sandbox_id": sb})).await;
    let echo = info["forwards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["guest_port"] == ECHO_PORT)
        .unwrap()
        .clone();
    assert_eq!(echo["accepted"], 17, "{info}");
    assert_eq!(
        echo["refused"],
        json!({"no_listener": 0, "cap": 0, "unclaimed": 0, "rejected": 0}),
        "{info}"
    );

    // A port that is not a forward is refused by the kernel; there is no route out.
    let r = bash(&socket, &sb, "exec 3<>/dev/tcp/127.0.0.1/15999; echo rc=$?").await;
    let tail = r.exit.as_ref().unwrap()["stderr_tail"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(tail.contains("Connection refused"), "{r:?}");

    let ok = Command::new("kill")
        .args(["-TERM", &server.0.id().to_string()])
        .status()
        .unwrap();
    assert!(ok.success());
    assert!(server.0.wait().unwrap().success());
}
