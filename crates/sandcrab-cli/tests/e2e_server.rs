//! `sandcrab server` on Docker, driven by a raw client over its socket: create, list,
//! destroy; then a server killed with a sandbox alive, and a new server that removes it.
//!
//! Needs a Docker daemon, the `base` template pinned on this host and a static `crabd`
//! (`$SANDCRAB_CRABD`, else `<target dir>/<arch>-unknown-linux-musl/release/crabd`; build it
//! with `scripts/build-crabd.sh`). With a private `SANDCRAB_HOME`, point
//! `SANDCRAB_TEMPLATE_PINS` at the host's pin store. Run with
//! `cargo test -p sandcrab-cli --test e2e_server -- --ignored`.

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
}

impl Client {
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
        assert_eq!(hello["backend"], "docker", "{hello}");
        c
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
        let r = loop {
            let mut h = [0u8; 4];
            self.s.read_exact(&mut h).await.unwrap();
            let mut body = vec![0u8; u32::from_be_bytes(h) as usize];
            self.s.read_exact(&mut body).await.unwrap();
            let r: Value = serde_json::from_slice(&body).unwrap();
            // Notifications (`sandbox.destroyed`, ...) carry no id: skip them.
            if r.get("id").is_some() {
                break r;
            }
        };
        r.get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{method}: {r}"))
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

/// `sandcrab server` with a private state dir; waits until it accepts.
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

fn containers_of(sandbox_id: &str) -> usize {
    let out = Command::new("docker")
        .args(["ps", "-aq", "--filter"])
        .arg(format!("label=sandcrab.sandbox_id={sandbox_id}"))
        .output()
        .expect("docker ps");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).lines().count()
}

#[tokio::test]
#[ignore = "e2e: needs Docker, the pinned base template and a static crabd"]
async fn server_on_docker_create_list_destroy_and_restart_reaps() {
    let tmp = tempfile::Builder::new()
        .prefix("sc-srv")
        .tempdir_in("/tmp")
        .unwrap();
    let state = tmp.path().join("state");
    let socket = tmp.path().join("sock/server.sock");

    let mut first = start(&state, &socket).await;
    let mut c = Client::connect(&socket).await;
    let t = Instant::now();
    let a = c
        .call(
            "sandbox.create",
            json!({"template": "base", "labels": {"e2e": "server"}}),
        )
        .await;
    eprintln!("create: {:?}", t.elapsed());
    let id_a = a["sandbox_id"].as_str().unwrap().to_owned();
    assert_eq!(containers_of(&id_a), 1);
    let list = c
        .call("sandbox.list", json!({"labels": {"e2e": "server"}}))
        .await;
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    let r = c.call("sandbox.destroy", json!({"sandbox_id": id_a})).await;
    assert_eq!(r, json!({"destroyed": true}));
    assert_eq!(containers_of(&id_a), 0);
    assert_eq!(c.call("sandbox.list", json!({})).await, json!([]));

    // A sandbox left behind by a server that dies.
    let b = c.call("sandbox.create", json!({"template": "base"})).await;
    let id_b = b["sandbox_id"].as_str().unwrap().to_owned();
    drop(c);
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    assert_eq!(containers_of(&id_b), 1, "the container outlives its server");

    // A new server removes it at start and lists nothing.
    let mut second = start(&state, &socket).await;
    assert_eq!(
        containers_of(&id_b),
        0,
        "the predecessor's sandbox is removed"
    );
    let mut c = Client::connect(&socket).await;
    assert_eq!(c.call("sandbox.list", json!({})).await, json!([]));

    // SIGTERM: every sandbox destroyed, the socket removed.
    let d = c.call("sandbox.create", json!({"template": "base"})).await;
    let id_d = d["sandbox_id"].as_str().unwrap().to_owned();
    let ok = Command::new("kill")
        .args(["-TERM", &second.0.id().to_string()])
        .status()
        .unwrap();
    assert!(ok.success());
    let status = second.0.wait().unwrap();
    assert!(status.success(), "{status}");
    assert_eq!(containers_of(&id_d), 0);
    assert!(!socket.exists());
}
