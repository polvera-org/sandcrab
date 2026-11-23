//! Backend-generic end-to-end harness for the SDK test suites (`tests/e2e_*.rs`).
//!
//! Every scenario talks to Sandcrab only through the public SDK, so the same suite runs on
//! any backend. The backend is picked by `SANDCRAB_E2E_BACKEND`:
//!
//! - `docker` (default): Docker with the explicit insecure opt-in. Needs a Docker daemon,
//!   the template images (`templates/build.sh --pin`) and a static `crabd` (`$SANDCRAB_CRABD`,
//!   else `target/<arch>-unknown-linux-musl/release/crabd`, built on first use).
//! - `firecracker`: not wired into the SDK yet → every test prints a SKIP line and passes.
//!   TODO(fc-wire, fc-e2e): once `SandcrabBuilder` can select the Firecracker backend, build it
//!   in [`Harness::new`] (the only backend-specific spot) and run this suite on a KVM host.
//!
//! All scenarios are `#[ignore]`d. Run with:
//!
//! ```text
//! cargo test -p sandcrab-sdk -- --ignored --test-threads=2
//! ```
//!
//! Each test gets its own temp state dir (workspace registry + sandbox sockets) and unique
//! workspace names; [`run`] destroys every sandbox the test spawned even when it panics.

#![allow(dead_code)]

use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use sandcrab_sdk::{
    BashParams, BashResult, DockerConfig, Sandbox, SandboxId, Sandcrab, SandcrabBuilder,
    WorkspaceClient,
};
use tokio::sync::RwLock;

/// Env var selecting the backend under test.
pub const BACKEND_ENV: &str = "SANDCRAB_E2E_BACKEND";

/// Tests that assert on host-wide state (e.g. "no managed containers left") take this
/// exclusively; every other test holds it shared.
static HOST_LOCK: RwLock<()> = RwLock::const_new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum E2eBackend {
    Docker,
    Firecracker,
}

impl E2eBackend {
    pub fn from_env() -> Self {
        match std::env::var(BACKEND_ENV).as_deref() {
            Err(_) | Ok("" | "docker") => Self::Docker,
            Ok("firecracker") => Self::Firecracker,
            Ok(other) => panic!("{BACKEND_ENV} must be `docker` or `firecracker`, got `{other}`"),
        }
    }
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Path of a static crabd, building it once if needed.
pub fn crabd() -> &'static Path {
    static CRABD: OnceLock<PathBuf> = OnceLock::new();
    CRABD.get_or_init(|| {
        if let Some(p) = std::env::var_os("SANDCRAB_CRABD").filter(|v| !v.is_empty()) {
            return PathBuf::from(p);
        }
        let default = repo_root().join(format!(
            "target/{}-unknown-linux-musl/release/crabd",
            std::env::consts::ARCH
        ));
        if default.is_file() {
            return default;
        }
        eprintln!("building static crabd via scripts/build-crabd.sh ...");
        let out = Command::new(repo_root().join("scripts/build-crabd.sh"))
            .current_dir(repo_root())
            .output()
            .expect("run scripts/build-crabd.sh");
        assert!(
            out.status.success(),
            "build-crabd.sh failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("path:").map(|p| PathBuf::from(p.trim())))
            .expect("build-crabd.sh prints the binary path")
    })
}

/// Docker config with the explicit opt-in and the test crabd.
pub fn docker_config(allow_insecure: bool) -> DockerConfig {
    DockerConfig {
        allow_insecure_dev_backend: allow_insecure,
        crabd_path: crabd().to_path_buf(),
        ..DockerConfig::default()
    }
}

/// One test's Sandcrab instance, temp state and naming scope.
pub struct Harness {
    pub sc: Sandcrab,
    pub backend: E2eBackend,
    tmp: tempfile::TempDir,
    scope: String,
}

impl Harness {
    /// `None` (after printing why) when the selected backend cannot run yet.
    pub fn new(test: &str) -> Option<Self> {
        let backend = E2eBackend::from_env();
        let tmp = tempfile::Builder::new()
            .prefix("sandcrab-e2e-")
            .tempdir()
            .unwrap();
        let builder = base_builder(tmp.path());
        let sc = match backend {
            E2eBackend::Docker => builder.docker(docker_config(true)).build().unwrap(),
            E2eBackend::Firecracker => {
                // TODO(fc-wire): build with the Firecracker backend here
                // (`builder.firecracker(..)` / `.backend(FirecrackerBackend::new(..))`).
                // TODO(fc-e2e): run this suite on a KVM host and drop this skip.
                eprintln!(
                    "SKIP {test}: {BACKEND_ENV}=firecracker is not wired into the SDK yet \
                     (pending flow steps fc-wire / fc-e2e)"
                );
                return None;
            }
        };
        let tag = SandboxId::new().to_string();
        let scope = format!("{}-{}", slug(test), &tag[..8]);
        Some(Self {
            sc,
            backend,
            tmp,
            scope,
        })
    }

    pub fn is_docker(&self) -> bool {
        self.backend == E2eBackend::Docker
    }

    /// Unique workspace name for this test (`[a-z0-9][a-z0-9-_]{0,62}`).
    pub fn name(&self, suffix: &str) -> String {
        let n = format!("{}-{}", self.scope, slug(suffix));
        n[..n.len().min(63)].to_owned()
    }

    pub fn ws(&self) -> WorkspaceClient {
        self.sc.workspaces()
    }

    /// Per-test scratch directory on the host (parent of the state dir).
    pub fn tmp(&self) -> &Path {
        self.tmp.path()
    }

    /// A builder sharing this test's state/templates but with no backend selected.
    pub fn builder(&self) -> SandcrabBuilder {
        base_builder(self.tmp.path())
    }

    /// Whether the backend still has a live instance for `id` (Docker: any container
    /// labelled with it). Other backends: the SDK's own view only.
    pub fn backend_has(&self, id: SandboxId) -> bool {
        match self.backend {
            E2eBackend::Docker => !docker_ps(&format!("sandcrab.sandbox_id={id}")).is_empty(),
            E2eBackend::Firecracker => self.sc.list_sandboxes().iter().any(|i| i.id == id),
        }
    }

    /// No sandbox tracked by the SDK and none of `ids` alive on the backend.
    pub fn assert_none_running(&self, ids: &[SandboxId]) {
        let live = self.sc.list_sandboxes();
        assert!(live.is_empty(), "sandboxes still tracked: {live:?}");
        for id in ids {
            assert!(
                !self.backend_has(*id),
                "sandbox {id} still alive on backend"
            );
        }
    }
}

fn base_builder(tmp: &Path) -> SandcrabBuilder {
    Sandcrab::builder()
        .state_dir(tmp.join("state"))
        .templates_dir(repo_root().join("templates"))
        .ready_timeout(Duration::from_secs(60))
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// IDs of containers (any state) matching a label filter (`docker ps -q --filter label=..`).
pub fn docker_ps(label: &str) -> Vec<String> {
    let out = Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("label={label}")])
        .output()
        .expect("run docker ps");
    assert!(out.status.success(), "docker ps failed: {out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Managed containers (any state) with a bind mount under `dir`.
pub fn docker_owned_by(dir: &Path) -> Vec<String> {
    let ids = docker_ps("sandcrab.managed=true");
    if ids.is_empty() {
        return ids;
    }
    let out = Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{.Id}} {{range .Mounts}}{{.Source}} {{end}}",
        ])
        .args(&ids)
        .output()
        .expect("run docker inspect");
    // A container may vanish between `ps` and `inspect`; parse whatever was printed.
    let needle = dir.to_string_lossy().into_owned();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(needle.as_str()))
        .filter_map(|l| l.split_whitespace().next().map(|id| id[..12].to_owned()))
        .collect()
}

/// Host uid (owner of a fresh temp dir).
pub fn host_uid() -> u32 {
    let d = tempfile::tempdir().unwrap();
    std::fs::metadata(d.path()).unwrap().uid()
}

/// Run a test body with a fresh [`Harness`]; tear down every sandbox afterwards, also when
/// the body panics (the panic is then re-raised).
pub async fn run<F, Fut>(test: &str, body: F)
where
    F: FnOnce(Arc<Harness>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let _shared = HOST_LOCK.read().await;
    run_inner(test, body).await;
}

/// Like [`run`], but with no other harness test of this binary running concurrently.
pub async fn run_exclusive<F, Fut>(test: &str, body: F)
where
    F: FnOnce(Arc<Harness>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let _excl = HOST_LOCK.write().await;
    run_inner(test, body).await;
}

async fn run_inner<F, Fut>(test: &str, body: F)
where
    F: FnOnce(Arc<Harness>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let Some(h) = Harness::new(test) else {
        return;
    };
    let h = Arc::new(h);
    let started = Instant::now();
    let outcome = tokio::spawn(body(h.clone())).await;
    if let Err(e) = h.sc.shutdown().await {
        eprintln!("{test}: cleanup error: {e}");
    }
    if h.is_docker() {
        // Backstop: `shutdown()` can return while a drop-triggered background destroy is
        // still tearing a container down (see e2e_lifecycle.rs). Wait for those, then
        // force-remove anything still mounting this test's temp dir.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut left = docker_owned_by(h.tmp());
        while !left.is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            left = docker_owned_by(h.tmp());
        }
        if !left.is_empty() {
            eprintln!("{test}: WARNING force-removing leaked containers {left:?}");
            let _ = Command::new("docker")
                .args(["rm", "-f"])
                .args(&left)
                .output();
        }
    }
    eprintln!("{test}: {:.1?}", started.elapsed());
    if let Err(e) = outcome {
        if e.is_panic() {
            std::panic::resume_unwind(e.into_panic());
        }
        panic!("{test}: body task failed: {e}");
    }
}

/// `bash` with an explicit timeout.
pub async fn bash_t(sb: &Sandbox, cmd: &str, timeout_ms: u64) -> BashResult {
    sb.bash_with(BashParams {
        command: cmd.into(),
        cwd: None,
        env: Default::default(),
        timeout_ms: Some(timeout_ms),
    })
    .await
    .unwrap_or_else(|e| panic!("bash `{cmd}`: {e}"))
}

/// `bash` that must exit 0; returns stdout.
pub async fn sh(sb: &Sandbox, cmd: &str) -> String {
    let r = sb
        .bash(cmd)
        .await
        .unwrap_or_else(|e| panic!("bash `{cmd}`: {e}"));
    assert_eq!(r.exit_code, Some(0), "`{cmd}` failed: {r:?}");
    r.stdout
}

/// Poll `check` every 200 ms until it returns `Some`, or panic after `timeout`.
pub async fn wait_for<T, F, Fut>(what: &str, timeout: Duration, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = check().await {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
