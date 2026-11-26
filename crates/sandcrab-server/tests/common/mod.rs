//! The test harness shared by the integration tests: a server on core's `FakeBackend` over a
//! real Unix socket, and a raw client that speaks the frames.

#![allow(dead_code)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sandcrab_core::testing::{FakeBackend, FakeEvent};
use sandcrab_sdk::Sandcrab;
use sandcrab_server::{BackendName, Server, ServerConfig, ServerError};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub fn me() -> u32 {
    sandcrab_sdk::core::private_dir::current_uid()
}

/// The effective gid, as a new file in a plain temporary directory shows it (Linux).
pub fn my_gid() -> u32 {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().metadata().unwrap().gid()
}

pub fn write_template(dir: &Path) {
    let d = dir.join("base");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(
        d.join("sandcrab.toml"),
        "[template]\nname = \"base\"\nversion = \"0.1.0\"\nimage = \"sandcrab/base:0.1.0\"\n\
         [resources]\nvcpu = 1\nmemory_mb = 512\ndisk_mb = 2048\n",
    )
    .unwrap();
}

pub struct Harness {
    pub tmp: tempfile::TempDir,
    pub socket: PathBuf,
    pub backend: FakeBackend,
    pub stop: Option<oneshot::Sender<()>>,
    pub task: Option<JoinHandle<Result<(), ServerError>>>,
}

pub fn base_config(tmp: &Path) -> ServerConfig {
    write_template(&tmp.join("templates"));
    ServerConfig {
        socket: Some(tmp.join("sock/server.sock")),
        backend: Some(BackendName::Docker),
        state_dir: Some(tmp.join("state")),
        templates_dirs: vec![tmp.join("templates")],
        ..ServerConfig::default()
    }
}

pub async fn try_start(
    config: ServerConfig,
    backend: &FakeBackend,
) -> Result<(Server, oneshot::Sender<()>, oneshot::Receiver<()>), ServerError> {
    let builder = Sandcrab::builder()
        .backend(backend.clone())
        .ready_timeout(Duration::from_secs(2));
    let server = Server::start(config, builder, Vec::new()).await?;
    let (tx, rx) = oneshot::channel();
    Ok((server, tx, rx))
}

impl Harness {
    pub async fn new(edit: impl FnOnce(&mut ServerConfig, &Path)) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = base_config(tmp.path());
        edit(&mut config, tmp.path());
        let socket = config.socket.clone().unwrap();
        let backend = FakeBackend::shared_kernel();
        let (server, tx, rx) = try_start(config, &backend).await.unwrap();
        let task = tokio::spawn(server.run(async move {
            let _ = rx.await;
        }));
        Self {
            tmp,
            socket,
            backend,
            stop: Some(tx),
            task: Some(task),
        }
    }

    pub async fn client(&self) -> Client {
        let mut c = Client::connect(&self.socket).await;
        let r = c.call("server.hello", hello("1.0")).await;
        assert!(r.get("result").is_some(), "{r}");
        c
    }

    pub async fn stop(&mut self) -> Result<(), ServerError> {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap()
    }

    pub fn destroyed(&self) -> usize {
        self.backend
            .events()
            .iter()
            .filter(|e| matches!(e, FakeEvent::Destroyed(_)))
            .count()
    }
}

pub fn hello(version: &str) -> Value {
    json!({"client": "test", "api_version": version})
}

pub struct Client {
    pub s: UnixStream,
    pub next: i64,
}

impl Client {
    pub async fn connect(path: &Path) -> Self {
        Self {
            s: UnixStream::connect(path).await.unwrap(),
            next: 1,
        }
    }

    pub async fn send(&mut self, v: &Value) {
        let body = serde_json::to_vec(v).unwrap();
        self.raw(&u32::try_from(body.len()).unwrap().to_be_bytes())
            .await;
        self.raw(&body).await;
    }

    pub async fn raw(&mut self, bytes: &[u8]) {
        self.s.write_all(bytes).await.unwrap();
    }

    /// The next frame, or `None` when the server closed.
    pub async fn recv(&mut self) -> Option<Value> {
        let read = async {
            let mut h = [0u8; 4];
            self.s.read_exact(&mut h).await.ok()?;
            let mut body = vec![0u8; u32::from_be_bytes(h) as usize];
            self.s.read_exact(&mut body).await.ok()?;
            Some(serde_json::from_slice(&body).unwrap())
        };
        tokio::time::timeout(Duration::from_secs(10), read)
            .await
            .expect("an answer or a close in time")
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        // Notifications may come first; they are skipped.
        loop {
            let r = self.recv().await.expect("an answer");
            if r.get("id").is_none() && r.get("method").is_some() {
                continue;
            }
            assert_eq!(r["id"], json!(id), "{r}");
            return r;
        }
    }

    pub async fn ok(&mut self, method: &str, params: Value) -> Value {
        let r = self.call(method, params).await;
        r.get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{method}: {r}"))
    }

    pub async fn err(&mut self, method: &str, params: Value) -> Value {
        let r = self.call(method, params).await;
        r.get("error")
            .cloned()
            .unwrap_or_else(|| panic!("{method} should fail: {r}"))
    }

    pub async fn closed(&mut self) -> bool {
        self.recv().await.is_none()
    }
}

pub fn code(e: &Value) -> i64 {
    e["code"].as_i64().unwrap()
}

// ---------------------------------------------------------------------------------------------
// Process and forward connections
// ---------------------------------------------------------------------------------------------

pub const STDIN: u8 = 0x01;
pub const STDIN_EOF: u8 = 0x02;
pub const STDOUT: u8 = 0x11;
pub const STDERR: u8 = 0x12;
pub const EXIT: u8 = 0x1F;

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The mock `crabd` of a sandbox the server created.
pub fn mock(h: &Harness, sandbox_id: &Value) -> sandcrab_core::testing::MockCrabd {
    let id = uuid::Uuid::parse_str(sandbox_id.as_str().unwrap()).unwrap();
    h.backend.mock_of(sandcrab_sdk::SandboxId(id)).unwrap()
}

/// `process.spawn` params for `argv` in `sandbox` with a fresh id, plus `extra` fields.
pub fn spawn_params(sandbox: &Value, argv: &[&str], extra: Value) -> Value {
    let mut p = json!({"process_id": new_id(), "sandbox_id": sandbox, "argv": argv, "env": {}});
    for (k, v) in extra.as_object().unwrap() {
        p[k] = v.clone();
    }
    p
}

impl Client {
    /// Open a process connection: send `process.spawn` and read the answer frame (`None`: the
    /// server closed without one).
    pub async fn spawn(path: &Path, params: &Value) -> (Self, Option<Value>) {
        let mut c = Self::connect(path).await;
        c.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "process.spawn", "params": params}))
            .await;
        let r = c.recv().await;
        (c, r)
    }

    /// A process connection that must be acked.
    pub async fn spawn_ok(path: &Path, params: &Value) -> Self {
        let (c, r) = Self::spawn(path, params).await;
        let r = r.expect("an answer to process.spawn");
        assert_eq!(
            r["result"]["process_id"], params["process_id"],
            "spawn acked: {r}"
        );
        c
    }

    pub async fn record(&mut self, kind: u8, payload: &[u8]) {
        let mut b = vec![kind];
        b.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        b.extend_from_slice(payload);
        self.s.write_all(&b).await.unwrap();
    }

    /// The next record, or `None` at a close.
    pub async fn next_record(&mut self) -> Option<(u8, Vec<u8>)> {
        let read = async {
            let mut h = [0u8; 5];
            self.s.read_exact(&mut h).await.ok()?;
            let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
            let mut p = vec![0u8; len];
            self.s.read_exact(&mut p).await.ok()?;
            Some((h[0], p))
        };
        tokio::time::timeout(Duration::from_secs(15), read)
            .await
            .expect("a record or a close in time")
    }

    /// Read records to the end: (stdout, stderr, the EXIT payload if any). Asserts that
    /// nothing follows `EXIT`.
    pub async fn drain(&mut self) -> (Vec<u8>, Vec<u8>, Option<Value>) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        while let Some((kind, p)) = self.next_record().await {
            match kind {
                STDOUT => out.extend_from_slice(&p),
                STDERR => err.extend_from_slice(&p),
                EXIT => {
                    let exit = serde_json::from_slice(&p).unwrap();
                    assert!(
                        self.next_record().await.is_none(),
                        "EXIT is the last record"
                    );
                    return (out, err, Some(exit));
                }
                other => panic!("unexpected record kind {other:#x}"),
            }
        }
        (out, err, None)
    }

    /// The next frame that is a notification of `method`, skipping others.
    pub async fn notification(&mut self, method: &str) -> Value {
        loop {
            let m = self.recv().await.expect("a notification");
            if m["method"] == method {
                return m["params"].clone();
            }
        }
    }
}
