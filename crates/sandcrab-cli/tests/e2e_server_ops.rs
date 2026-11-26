//! `sandcrab server` on Docker, operations: `server.selftest` on `base` through the socket
//! (every line passes), the report kept for `server.capabilities` and returned again within
//! `max_age_secs`; `sandbox.destroyed` with reason `ttl` when the TTL ends a sandbox.
//!
//! Needs a Docker daemon, the `base` template pinned on this host and a static `crabd`
//! (`$SANDCRAB_CRABD`, else `<target dir>/<arch>-unknown-linux-musl/release/crabd`). With a
//! private `SANDCRAB_HOME`, point `SANDCRAB_TEMPLATE_PINS` at the host's pin store. Run with
//! `cargo test -p sandcrab-cli --test e2e_server_ops -- --ignored`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Client {
    s: UnixStream,
    next: i64,
    notes: Vec<Value>,
}

impl Client {
    async fn connect(path: &Path) -> Self {
        let mut c = Self {
            s: UnixStream::connect(path).await.unwrap(),
            next: 1,
            notes: Vec::new(),
        };
        let hello = c
            .call(
                "server.hello",
                json!({"client": "e2e", "api_version": "1.0"}),
            )
            .await;
        assert_eq!(hello["backend"], "docker", "{hello}");
        assert!(
            hello["features"]
                .as_array()
                .unwrap()
                .contains(&json!("selftest")),
            "{hello}"
        );
        c
    }

    async fn frame(&mut self) -> Value {
        let mut h = [0u8; 4];
        self.s.read_exact(&mut h).await.unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(h) as usize];
        self.s.read_exact(&mut body).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let body = serde_json::to_vec(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .unwrap();
        self.s
            .write_all(&u32::try_from(body.len()).unwrap().to_be_bytes())
            .await
            .unwrap();
        self.s.write_all(&body).await.unwrap();
        let r = tokio::time::timeout(Duration::from_secs(240), async {
            loop {
                let r = self.frame().await;
                if r.get("id").is_some() {
                    break r;
                }
                self.notes.push(r);
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{method}: no answer in time"));
        r.get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{method}: {r}"))
    }

    /// The next `method` notification (kept ones first), within `limit`.
    async fn notification(&mut self, method: &str, limit: Duration) -> Value {
        if let Some(i) = self.notes.iter().position(|n| n["method"] == method) {
            return self.notes.remove(i)["params"].clone();
        }
        tokio::time::timeout(limit, async {
            loop {
                let n = self.frame().await;
                if n["method"] == method {
                    break n["params"].clone();
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {method} in {limit:?}"))
    }
}

/// A `sandcrab server` child that cannot outlive the test (`--lifeline-stdin`, killed and
/// reaped on drop).
struct ServerChild(Child);

impl Drop for ServerChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start(state: &Path, socket: &Path, root: &Path) -> ServerChild {
    let child = Command::new(env!("CARGO_BIN_EXE_sandcrab"))
        .arg("--backend=docker")
        .arg("--state-dir")
        .arg(state)
        .arg("--templates-dir")
        .arg(repo_root().join("templates"))
        .arg("server")
        .arg("--socket")
        .arg(socket)
        .arg("--mount-root")
        .arg(format!("{}:rw", root.display()))
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

#[tokio::test]
#[ignore = "e2e: needs Docker, the pinned base template and a static crabd"]
async fn server_on_docker_selftest_and_ttl() {
    let tmp = tempfile::Builder::new()
        .prefix("sc-ops")
        .tempdir_in("/tmp")
        .unwrap();
    let state = tmp.path().join("state");
    let socket = tmp.path().join("sock/server.sock");
    let root = tmp.path().join("mounts");
    std::fs::create_dir(&root).unwrap();
    let _server = start(&state, &socket, &root).await;
    let mut c = Client::connect(&socket).await;

    let caps = c
        .call("server.capabilities", json!({"template": "base"}))
        .await;
    assert_eq!(caps["selftest"], Value::Null, "never run: {caps}");

    let t = Instant::now();
    let r = c.call("server.selftest", json!({"template": "base"})).await;
    eprintln!("server.selftest: {:?}", t.elapsed());
    for l in r["lines"].as_array().unwrap() {
        eprintln!("  {:<12} {:<26} {}", l["status"], l["id"], l["detail"]);
    }
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["reason"], Value::Null);
    assert_eq!(r["backend"], "docker");
    let lines = r["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 15, "{r}");
    assert!(lines.iter().all(|l| l["status"] == "pass"), "{r}");
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        0,
        "scratch directory removed"
    );

    // Kept: the summary, and the same report within max_age.
    let caps = c
        .call("server.capabilities", json!({"template": "base"}))
        .await;
    assert_eq!(caps["selftest"]["ok"], true, "{caps}");
    assert_eq!(caps["selftest"]["template"], "base");
    let t = Instant::now();
    let again = c
        .call(
            "server.selftest",
            json!({"template": "base", "max_age_secs": 600}),
        )
        .await;
    assert_eq!(again, r);
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    // Its sandboxes are not any client's.
    assert_eq!(c.call("sandbox.list", json!({})).await, json!([]));

    // A TTL ends a sandbox: its owner hears `ttl`.
    let sb = c
        .call("sandbox.create", json!({"template": "base", "ttl_secs": 2}))
        .await["sandbox_id"]
        .clone();
    let note = c
        .notification("sandbox.destroyed", Duration::from_secs(30))
        .await;
    assert_eq!(note, json!({"sandbox_id": sb, "reason": "ttl"}));
    assert_eq!(c.call("sandbox.list", json!({})).await, json!([]));
}
