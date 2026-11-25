//! Spawn → helper config → `ctl.sock` readiness → `init.configure` → tools → destroy of the
//! real `LibkrunBackend`, against a **fake `sandcrab-krun`**. No hypervisor, no libkrun, no
//! Docker: runs in plain `cargo test` on macOS and Linux.
//!
//! This test binary has its own `main` (`harness = false`) and plays three roles:
//! - started as `…/sandcrab-krun` (a symlink to this executable, so `argv[0]` picks the role:
//!   the backend clears the helper's environment) it **is the fake helper**;
//! - started with `--fake-owner <home>` it is a short-lived **owner process** that spawns one
//!   sandbox and exits without tearing it down (lifeline + reaper tests);
//! - otherwise it runs the tests.
//!
//! The fake helper, like the real one (`src/bin/sandcrab-krun.rs`):
//! - reads one `HelperConfig` line from stdin and validates it with the real
//!   `helper_proto` (exit 70 on failure), and checks that the disk images exist;
//! - watches the lifeline: the rest of stdin is read by a thread, EOF → SIGKILL itself;
//! - binds the vsock-mapped Unix sockets of the config: `ctl.sock` is a minimal `crabd init`
//!   (`ping`, `init.configure`, `init.shutdown` → exit), `v.sock` is core's `MockCrabd`
//!   (only after `init.configure`, like the real guest).
//!
//! Everything it sees (config, environment, pid / pgid / cwd, control calls, signals) is
//! appended to `<run_dir>/fake-log/<sandbox id>.jsonl` for the assertions. Failure modes come
//! from `<run_dir>/fake-mode` (see [`fake::Mode`]).

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sandcrab_backend_libkrun::helper_proto::{
    CONTROL_PORT, GuestExec, HelperConfig, HelperNetwork, HostIdentity, ROOT_DRIVE_ID,
    SCRATCH_DRIVE_ID, TOOLS_PORT, exit_code,
};
use sandcrab_backend_libkrun::netstack::NetstackKind;
use sandcrab_backend_libkrun::reap::{ProcId, RunMeta};
use sandcrab_backend_libkrun::verified::HelperRecord;
use sandcrab_backend_libkrun::{KrunLibs, LibkrunBackend, LibkrunConfig, RootfsSource};
use sandcrab_core::testing::MockCrabd;
use sandcrab_core::{
    LifecycleTimings, ManagerConfig, MountMode, NetworkConfig, PinStore, ResolvedSpec,
    RuntimeBackend, SandboxConfig, SandboxError, SandboxId, SandboxManager, Template,
    TemplateRegistry, WorkspaceRegistry, WorkspaceState,
};
use sandcrab_protocol::init::{GuestMount, InitConfig, METHOD_CONFIGURE, METHOD_SHUTDOWN};
use sandcrab_protocol::{
    BashParams, BashResult, HEADER_LEN, METHOD_PING, RpcMessage, RpcResponse, encode_frame,
    parse_header,
};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// The helper's file name: `argv[0]` ending in this selects the fake-helper role.
const HELPER_NAME: &str = "sandcrab-krun";
/// What the backend exports so libkrun finds libkrunfw (the only variable the helper gets).
#[cfg(target_os = "macos")]
const LIB_PATH_ENV: &str = "DYLD_FALLBACK_LIBRARY_PATH";
#[cfg(not(target_os = "macos"))]
const LIB_PATH_ENV: &str = "LD_LIBRARY_PATH";
/// Disk size of the `base` template (the macOS scratch cache is keyed by it).
const DISK_MB: u64 = 2048;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args.first().map(String::as_str).unwrap_or_default();
    if Path::new(argv0)
        .file_name()
        .is_some_and(|n| n == HELPER_NAME)
    {
        fake::main();
    }
    if let Some(i) = args.iter().position(|a| a == "--fake-owner") {
        return owner_main(Path::new(&args[i + 1]));
    }
    harness()
}

// ---------------------------------------------------------------------------------------
// The fake helper.
// ---------------------------------------------------------------------------------------

mod fake {
    use super::*;

    /// Behaviour switches (`<run_dir>/fake-mode`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mode {
        /// A healthy guest.
        Normal,
        /// A `krun_*` call fails before the VM starts: exit 73.
        Exit73,
        /// Never listens on anything (lifeline still honoured).
        Hang,
        /// Accepts every control connection and closes it at once (libkrun before the guest
        /// listens), forever.
        AcceptClose,
        /// Like `AcceptClose` for the first three connections, then healthy.
        FlakyAccept,
        /// Acks `init.shutdown` but keeps running (SIGTERM still kills it).
        IgnoreShutdown,
        /// Ignores `init.shutdown`, SIGTERM and the lifeline: only SIGKILL stops it.
        Stubborn,
        /// Healthy, but does not watch the lifeline (an orphan the reaper must kill).
        IgnoreLifeline,
        /// Healthy until `<sandbox dir>/crash-now` appears, then exits 1 (VM crash).
        CrashOnSignal,
    }

    impl Mode {
        fn parse(s: &str) -> Self {
            match s.trim() {
                "" | "normal" => Self::Normal,
                "exit73" => Self::Exit73,
                "hang" => Self::Hang,
                "accept_close" => Self::AcceptClose,
                "flaky_accept" => Self::FlakyAccept,
                "ignore_shutdown" => Self::IgnoreShutdown,
                "stubborn" => Self::Stubborn,
                "ignore_lifeline" => Self::IgnoreLifeline,
                "crash_on_signal" => Self::CrashOnSignal,
                other => panic!("unknown fake mode `{other}`"),
            }
        }
    }

    /// Appends JSON lines to `<run_dir>/fake-log/<id>.jsonl`.
    pub struct Recorder {
        file: Mutex<fs::File>,
    }

    impl Recorder {
        pub fn record(&self, v: Value) {
            let mut line = v.to_string();
            line.push('\n');
            let mut f = self.file.lock().unwrap();
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn log(msg: impl std::fmt::Display) {
        eprintln!("sandcrab-krun(fake): {msg}");
    }

    fn fail(code: i32, msg: impl std::fmt::Display) -> ! {
        log(format_args!(
            "error: {msg} (exit {code}: {})",
            exit_code::describe(code).unwrap_or("?")
        ));
        std::process::exit(code);
    }

    pub fn main() -> ! {
        let cfg = match HelperConfig::read_from(std::io::stdin().lock()) {
            Ok(c) => c,
            Err(e) => fail(exit_code::CONFIG_INVALID, e),
        };
        let sandbox_dir = cfg.control_socket.parent().unwrap().to_path_buf();
        let run_dir = sandbox_dir.parent().unwrap().to_path_buf();
        let id = sandbox_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mode = Mode::parse(&fs::read_to_string(run_dir.join("fake-mode")).unwrap_or_default());
        fs::create_dir_all(run_dir.join("fake-log")).unwrap();
        let rec = Arc::new(Recorder {
            file: Mutex::new(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(run_dir.join("fake-log").join(format!("{id}.jsonl")))
                    .unwrap(),
            ),
        });
        let env: BTreeMap<String, String> = std::env::vars().collect();
        rec.record(json!({
            "event": "start",
            "proc": ProcId::current(),
            "pgid": rustix::process::getpgrp().as_raw_pid(),
            "cwd": std::env::current_dir().unwrap(),
            "env": env,
            "config": cfg,
        }));

        if !matches!(mode, Mode::IgnoreLifeline | Mode::Stubborn) {
            start_lifeline(Arc::clone(&rec));
        }
        for d in [&cfg.root, &cfg.scratch].into_iter().chain(&cfg.disks) {
            if !d.path.is_file() {
                fail(
                    exit_code::CONFIG_INVALID,
                    format_args!("disk `{}` is not a regular file", d.path.display()),
                );
            }
        }
        match mode {
            Mode::Exit73 => {
                log("krun_set_vm_config(ctx 0, vcpus 1, 512 MiB) = -22 (EINVAL)");
                fail(
                    exit_code::LIBKRUN_CONFIG,
                    "krun_set_vm_config: Invalid argument",
                );
            }
            Mode::Hang => loop {
                std::thread::sleep(Duration::from_secs(3600));
            },
            _ => {}
        }
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(serve(cfg, mode, rec, sandbox_dir));
        std::process::exit(0);
    }

    /// The rest of stdin is the lifeline: EOF → the owner is gone → SIGKILL ourselves.
    fn start_lifeline(rec: Arc<Recorder>) {
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 64];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            rec.record(json!({"event": "lifeline_eof"}));
            log("lifeline closed (owner gone): killing the VM");
            let _ = rustix::process::kill_process(
                rustix::process::getpid(),
                rustix::process::Signal::KILL,
            );
            std::thread::sleep(Duration::from_secs(1));
            std::process::abort();
        });
    }

    async fn serve(cfg: HelperConfig, mode: Mode, rec: Arc<Recorder>, sandbox_dir: PathBuf) {
        if matches!(mode, Mode::Stubborn) {
            // Replacing the default action: SIGTERM no longer terminates us.
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            let rec = Arc::clone(&rec);
            tokio::spawn(async move {
                while term.recv().await.is_some() {
                    rec.record(json!({"event": "sigterm_ignored"}));
                    log("SIGTERM ignored");
                }
            });
        }
        // Like a booting VMM, the sockets appear a little after exec.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let ctl = UnixListener::bind(&cfg.control_socket).unwrap();
        let tools = UnixListener::bind(&cfg.tools_socket).unwrap();
        log(format_args!(
            "vsock {CONTROL_PORT} -> {}, {TOOLS_PORT} -> {}",
            cfg.control_socket.display(),
            cfg.tools_socket.display()
        ));
        let configured = Arc::new(AtomicBool::new(false));

        let ready = Arc::clone(&configured);
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = tools.accept().await else {
                    return;
                };
                // Before init.configure, `crabd serve` does not exist yet.
                if ready.load(Ordering::SeqCst) {
                    tokio::spawn(MockCrabd::default().serve(s));
                }
            }
        });

        if mode == Mode::CrashOnSignal {
            let trigger = sandbox_dir.join("crash-now");
            let rec = Arc::clone(&rec);
            tokio::spawn(async move {
                while !trigger.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                rec.record(json!({"event": "crash"}));
                log("guest kernel panic (fake)");
                std::process::exit(1);
            });
        }

        let accepted = AtomicU32::new(0);
        loop {
            let (s, _) = ctl.accept().await.unwrap();
            let n = accepted.fetch_add(1, Ordering::SeqCst);
            if mode == Mode::AcceptClose || (mode == Mode::FlakyAccept && n < 3) {
                drop(s); // libkrun's accept-then-close before the guest listens
                continue;
            }
            tokio::spawn(control(s, mode, Arc::clone(&rec), Arc::clone(&configured)));
        }
    }

    async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Option<Vec<u8>> {
        let mut h = [0u8; HEADER_LEN];
        r.read_exact(&mut h).await.ok()?;
        let mut p = vec![0u8; parse_header(h).ok()?];
        r.read_exact(&mut p).await.ok()?;
        Some(p)
    }

    async fn reply<W: AsyncWrite + Unpin>(w: &mut W, r: RpcResponse) {
        let _ = w.write_all(&encode_frame(&r).unwrap()).await;
    }

    /// Minimal `crabd init` control server.
    async fn control(
        mut s: UnixStream,
        mode: Mode,
        rec: Arc<Recorder>,
        configured: Arc<AtomicBool>,
    ) {
        while let Some(p) = read_frame(&mut s).await {
            let Ok(RpcMessage::Request(req)) = RpcMessage::from_slice(&p) else {
                continue;
            };
            let params = req.params.clone().unwrap_or(Value::Null);
            rec.record(json!({"control": req.method, "params": params}));
            match req.method.as_str() {
                METHOD_PING => reply(&mut s, RpcResponse::success(req.id, json!({}))).await,
                METHOD_CONFIGURE => {
                    let _: InitConfig = serde_json::from_value(params).unwrap();
                    configured.store(true, Ordering::SeqCst);
                    reply(
                        &mut s,
                        RpcResponse::success(req.id, json!({"serve_pid": 2, "boot_ms": 42})),
                    )
                    .await;
                }
                METHOD_SHUTDOWN => {
                    reply(&mut s, RpcResponse::success(req.id, json!({}))).await;
                    if !matches!(mode, Mode::IgnoreShutdown | Mode::Stubborn) {
                        // The guest syncs and reboots; libkrun exits with its status.
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        std::process::exit(0);
                    }
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Shared fixture.
// ---------------------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn base_template() -> Template {
    TemplateRegistry::load(&[repo_root().join("templates")])
        .unwrap()
        .get("base")
        .unwrap()
        .clone()
}

/// Backend config + libs rooted at `home` (short, under `/tmp`: the macOS `$TMPDIR` is too
/// long for the 103-byte socket path budget). Idempotent, so an owner process can rebuild it.
fn backend_at(home: &Path, tweak: impl FnOnce(&mut LibkrunConfig)) -> LibkrunBackend {
    backend_libs_at(home, tweak, |_| {})
}

fn backend_libs_at(
    home: &Path,
    tweak: impl FnOnce(&mut LibkrunConfig),
    tweak_libs: impl FnOnce(&mut KrunLibs),
) -> LibkrunBackend {
    let bin = home.join("bin");
    let helper = bin.join(HELPER_NAME);
    if !helper.exists() {
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &helper).unwrap();
        // The install record `sandcrab libkrun install` writes (hash the test binary once per
        // process): the backend then checks the helper with one `fstat` per spawn.
        static RECORD: std::sync::OnceLock<HelperRecord> = std::sync::OnceLock::new();
        RECORD
            .get_or_init(|| HelperRecord::record(&helper).unwrap())
            .save(&helper)
            .unwrap();
    }
    let lib = home.join("lib");
    fs::create_dir_all(&lib).unwrap();
    let (libkrun, libkrunfw) = (lib.join("libkrun.fake"), lib.join("libkrunfw.fake"));
    fs::write(&libkrun, b"not really libkrun").unwrap();
    fs::write(&libkrunfw, b"not really libkrunfw").unwrap();
    let rootfs = home.join("rootfs.ext4");
    fs::write(&rootfs, b"not really ext4").unwrap();
    // macOS copies a cached blank scratch image (mke2fs only on a miss); seed it so the
    // suite needs no e2fsprogs there. Linux always runs mke2fs (checked in `harness`).
    let blank = home.join("cache/krun-scratch");
    fs::create_dir_all(&blank).unwrap();
    fs::write(
        blank.join(format!(
            "blank-{DISK_MB}-v{}.ext4",
            sandcrab_rootfs::SCRATCH_FORMAT_VERSION
        )),
        b"blank",
    )
    .unwrap();

    let mut cfg = LibkrunConfig::with_home(home, "/unused/crabd");
    cfg.helper = helper;
    cfg.rootfs = RootfsSource::Image(rootfs);
    cfg.run_dir = home.join("run");
    cfg.boot_timeout = Duration::from_secs(5);
    cfg.term_after = Duration::from_millis(300);
    cfg.kill_after = Duration::from_millis(2000);
    tweak(&mut cfg);
    let mut libs = KrunLibs::hash_files(&libkrun, &libkrunfw).unwrap();
    tweak_libs(&mut libs);
    LibkrunBackend::new_unchecked(cfg, libs).unwrap()
}

/// A spec without mounts for direct `RuntimeBackend::spawn` calls.
fn bare_spec() -> ResolvedSpec {
    let template = base_template();
    let manifest = &template.manifest;
    ResolvedSpec {
        id: SandboxId::new(),
        image: template.image().to_owned(),
        image_sha256: None,
        mounts: Vec::new(),
        default_cwd: manifest.workspace.workdir.clone(),
        env: BTreeMap::new(),
        network: NetworkConfig::disabled(),
        ports: Vec::new(),
        user: None,
        resources: manifest.resources,
        ttl: None,
        secrets: Vec::new(),
        timings: LifecycleTimings::start(),
        template,
    }
}

struct Env {
    tmp: tempfile::TempDir,
    backend: Arc<LibkrunBackend>,
    manager: Option<SandboxManager>,
    workspaces: WorkspaceRegistry,
    project: PathBuf,
    docs: PathBuf,
}

impl Env {
    fn home(&self) -> &Path {
        self.tmp.path()
    }

    fn m(&self) -> &SandboxManager {
        self.manager.as_ref().expect("manager alive")
    }

    fn run_dir(&self) -> PathBuf {
        self.home().join("run")
    }

    fn set_mode(&self, mode: &str) {
        fs::create_dir_all(self.run_dir()).unwrap();
        fs::write(self.run_dir().join("fake-mode"), mode).unwrap();
    }

    /// Everything every fake helper of this env recorded.
    fn log(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(self.run_dir().join("fake-log")) else {
            return out;
        };
        for e in rd.flatten() {
            for line in fs::read_to_string(e.path()).unwrap().lines() {
                out.push(serde_json::from_str(line).unwrap());
            }
        }
        out
    }

    fn starts(&self) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|v| v["event"] == "start")
            .collect()
    }

    /// The single fake helper this env launched.
    fn start(&self) -> Value {
        let s = self.starts();
        assert_eq!(s.len(), 1, "exactly one helper launched: {s:?}");
        s.into_iter().next().unwrap()
    }

    fn helper(&self) -> ProcId {
        serde_json::from_value(self.start()["proc"].clone()).unwrap()
    }

    fn config(&self) -> HelperConfig {
        serde_json::from_value(self.start()["config"].clone()).unwrap()
    }

    fn has_event(&self, event: &str) -> bool {
        self.log().iter().any(|v| v["event"] == event)
    }

    fn control_methods(&self) -> Vec<String> {
        self.log()
            .iter()
            .filter_map(|v| v["control"].as_str().map(str::to_owned))
            .collect()
    }

    fn init_config(&self) -> InitConfig {
        let v = self
            .log()
            .into_iter()
            .find(|v| v["control"] == METHOD_CONFIGURE)
            .expect("init.configure sent");
        serde_json::from_value(v["params"].clone()).unwrap()
    }

    /// Sandbox run dirs left behind (ignores the fake's own files).
    fn run_dirs(&self) -> Vec<PathBuf> {
        fs::read_dir(self.run_dir())
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        let n = p.file_name().unwrap().to_string_lossy();
                        n != "fake-log" && n != "fake-mode"
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn state(&self, name: &str) -> WorkspaceState {
        self.workspaces.get(&name.into()).unwrap().state
    }

    /// Wait until both workspaces are `Idle` again (leases released).
    async fn leases_released(&self) -> bool {
        for _ in 0..300 {
            if self.state("project") == WorkspaceState::Idle
                && self.state("docs") == WorkspaceState::Idle
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    fn spawn_cfg(&self) -> SandboxConfig {
        SandboxConfig::from_template("base")
            .mount("project", "/workspace", MountMode::ReadWrite)
            .mount("docs", "/docs", MountMode::ReadOnly)
    }
}

impl Drop for Env {
    /// Never leave a fake helper behind, even when a test fails half-way.
    fn drop(&mut self) {
        for s in self.starts() {
            if let Ok(p) = serde_json::from_value::<ProcId>(s["proc"].clone()) {
                p.signal(rustix::process::Signal::KILL, true);
            }
        }
    }
}

fn env_with(tweak: impl FnOnce(&mut LibkrunConfig)) -> Env {
    env_libs(tweak, |_| {})
}

fn env_libs(tweak: impl FnOnce(&mut LibkrunConfig), tweak_libs: impl FnOnce(&mut KrunLibs)) -> Env {
    let tmp = tempfile::Builder::new()
        .prefix("lkfh")
        .tempdir_in("/tmp")
        .unwrap();
    let home = tmp.path();
    let backend = Arc::new(backend_libs_at(home, tweak, tweak_libs));
    let workspaces = WorkspaceRegistry::open(home.join("state")).unwrap();
    let project = home.join("project");
    let docs = home.join("docs");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&docs).unwrap();
    fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(docs.join("README.md"), "read me\n").unwrap();
    workspaces.register_host_dir("project", &project).unwrap();
    workspaces.register_host_dir("docs", &docs).unwrap();
    workspaces.create("notes").unwrap();
    let templates = TemplateRegistry::load(&[repo_root().join("templates")]).unwrap();
    let manager = SandboxManager::new(
        Arc::clone(&backend) as Arc<dyn RuntimeBackend>,
        workspaces.clone(),
        templates,
        ManagerConfig {
            ready_timeout: Duration::from_secs(5),
            template_pins: Some(PinStore::open(home.join("pins"))),
            ..ManagerConfig::default()
        },
    )
    .unwrap();
    Env {
        project: project.canonicalize().unwrap(),
        docs: docs.canonicalize().unwrap(),
        tmp,
        backend,
        manager: Some(manager),
        workspaces,
    }
}

fn env() -> Env {
    env_with(|_| {})
}

/// Wait until no process with this identity remains.
async fn gone(p: &ProcId) -> bool {
    for _ in 0..300 {
        if !p.is_alive() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

fn identity(dir: &Path) -> HostIdentity {
    let m = fs::metadata(dir).unwrap();
    HostIdentity {
        dev: m.dev(),
        ino: m.ino(),
    }
}

fn bash(cmd: &str) -> BashParams {
    BashParams {
        command: cmd.into(),
        cwd: None,
        env: Default::default(),
        timeout_ms: None,
    }
}

// ---------------------------------------------------------------------------------------
// The owner process (lifeline / reaper tests).
// ---------------------------------------------------------------------------------------

/// Spawn one sandbox on the backend at `home`, then exit without destroying it: no `Drop`
/// runs, so only the lifeline (and later the reaper) can clean up.
fn owner_main(home: &Path) -> ExitCode {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let backend = backend_at(home, |_| {});
    match rt.block_on(backend.spawn(&bare_spec())) {
        Ok(handle) => {
            println!("spawned {}", handle.id());
            std::io::stdout().flush().unwrap();
            std::process::exit(0);
        }
        Err(e) => {
            println!("spawn failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Run an owner process against `e`'s home and return the run dir it left behind.
fn run_owner(e: &Env) -> PathBuf {
    let out = Command::new(std::env::current_exe().unwrap())
        .arg("--fake-owner")
        .arg(e.home())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "owner: {stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = stdout.trim().strip_prefix("spawned ").unwrap().to_owned();
    let dir = e.run_dir().join(id);
    assert!(dir.is_dir(), "owner left its run dir behind");
    dir
}

// ---------------------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------------------

/// Spawn → readiness → tool call → destroy, plus everything the helper was handed.
async fn full_lifecycle() {
    let e = env();
    let sb = e
        .m()
        .spawn(
            e.spawn_cfg()
                .mount("notes", "/notes", MountMode::ReadWrite)
                .env("FOO", "bar")
                .require_hardware_isolation(true),
        )
        .await
        .unwrap();

    // Tools go through MockCrabd on v.sock.
    let r: BashResult = sb.call(bash("echo hi")).await.unwrap();
    assert_eq!(r.stdout, "echo hi");

    // Leases are held while it runs.
    assert_eq!(
        e.state("project"),
        WorkspaceState::LeasedRw { sandbox: sb.id() }
    );
    assert_eq!(e.state("docs"), WorkspaceState::LeasedRo { holders: 1 });

    // Process: own group, cwd = run dir, environment = only the libkrunfw dir.
    let start = e.start();
    let helper = e.helper();
    let dir = e.run_dir().join(sb.id().to_string());
    assert_eq!(start["pgid"].as_i64(), Some(i64::from(helper.pid)));
    assert_eq!(
        Path::new(start["cwd"].as_str().unwrap()),
        dir.canonicalize().unwrap()
    );
    let mut env: BTreeMap<String, String> = serde_json::from_value(start["env"].clone()).unwrap();
    // Set by CoreFoundation in every macOS process at startup, not inherited.
    env.remove("__CF_USER_TEXT_ENCODING");
    let lib_dir = e.home().join("lib");
    assert_eq!(
        env,
        BTreeMap::from([(LIB_PATH_ENV.to_owned(), lib_dir.display().to_string())]),
        "cleared environment, libkrunfw dir only (DYLD_* survives a Rust helper on macOS)"
    );

    // HelperConfig.
    let c = e.config();
    let libs = e.backend.libs();
    assert_eq!(
        (c.libkrun.clone(), c.libkrunfw.clone()),
        (libs.libkrun.clone(), libs.libkrunfw.clone())
    );
    assert_eq!((c.vcpus, c.memory_mib), (1, 512));
    assert_eq!(c.root.block_id, ROOT_DRIVE_ID);
    assert!(c.root.read_only);
    assert_eq!(c.root.path, e.home().join("rootfs.ext4"));
    assert_eq!(c.scratch.block_id, SCRATCH_DRIVE_ID);
    assert!(!c.scratch.read_only);
    assert_eq!(c.scratch.path, dir.join("scratch.ext4"));
    assert!(c.disks.is_empty());
    assert_eq!(c.control_socket, dir.join("ctl.sock"));
    assert_eq!(c.tools_socket, dir.join("v.sock"));
    assert_eq!(c.network, HelperNetwork::Disabled, "TSI off");
    assert_eq!(c.exec, GuestExec::default(), "fixed crabd init argv/env");
    assert_eq!(c.log_level, 0);
    // One live share per dir workspace: `ws<n>` by mount index, ro enforced host-side, and
    // the lease's canonical root + (dev, ino) for the helper's `/dev/fd` re-check.
    let notes = e.workspaces.get(&"notes".into()).unwrap();
    let notes_root = notes.backing.path().canonicalize().unwrap();
    let shares: Vec<_> = c
        .virtiofs
        .iter()
        .map(|s| (s.tag.as_str(), s.host_path.clone(), s.read_only, s.identity))
        .collect();
    assert_eq!(
        shares,
        [
            ("ws0", e.project.clone(), false, Some(identity(&e.project))),
            ("ws1", e.docs.clone(), true, Some(identity(&e.docs))),
            (
                "ws2",
                notes_root.clone(),
                false,
                Some(identity(&notes_root))
            ),
        ]
    );

    // init.configure.
    let init = e.init_config();
    assert_eq!(
        init.mounts,
        [
            GuestMount::virtio_fs("ws0", "/workspace", false),
            GuestMount::virtio_fs("ws1", "/docs", true),
            GuestMount::virtio_fs("ws2", "/notes", false),
        ]
    );
    assert_eq!(init.default_cwd, PathBuf::from("/workspace"));
    assert_eq!(init.env["FOO"], "bar");
    assert!(!init.network && init.dns.is_empty());
    assert_eq!(init.tools_port, TOOLS_PORT);
    assert_eq!(init.sandbox_id, sb.id().to_string());
    assert_eq!(
        (init.user.uid, init.user.gid),
        (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw()
        )
    );

    // Run dir + meta while running.
    assert_eq!(e.run_dirs(), std::slice::from_ref(&dir));
    let meta = RunMeta::load(&dir).unwrap();
    assert_eq!(meta.guest_boot_ms, Some(42));
    assert_eq!(meta.helper.as_ref(), Some(&helper));
    assert_eq!(meta.owner, ProcId::current());
    assert!(helper.is_alive());
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );

    // Destroy: init.shutdown stops the guest, well before the SIGTERM step.
    let t0 = Instant::now();
    let marks = sb.destroy().await.unwrap();
    assert!(
        t0.elapsed() < Duration::from_millis(300),
        "{:?}",
        t0.elapsed()
    );
    let names: Vec<_> = marks.custom.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "rootfs_ready",
            "ws_attach_done",
            "helper_started",
            "control_connected",
            "init_configured",
            "tools_ready",
            "vmm_exited",
            "cleanup_done",
        ]
    );
    assert!(marks.backend_started.is_some() && marks.ready.is_some());
    assert!(marks.backend_started <= marks.ready);
    let methods: Vec<_> = e
        .control_methods()
        .into_iter()
        .filter(|m| m != METHOD_PING)
        .collect();
    assert_eq!(methods, [METHOD_CONFIGURE, METHOD_SHUTDOWN]);
    assert!(
        e.control_methods().iter().any(|m| m == METHOD_PING),
        "readiness probe"
    );
    assert!(gone(&helper).await, "helper exited");
    assert!(e.run_dirs().is_empty(), "run dir removed");
    assert!(
        !e.has_event("lifeline_eof"),
        "exited on shutdown, not the lifeline"
    );
    assert!(e.leases_released().await, "leases released after destroy");
    assert_eq!(e.state("notes"), WorkspaceState::Idle);
}

/// A helper that dies before the guest is ready fails the spawn at once with the exit-code
/// meaning and the `vmm.log` tail; leases are released.
async fn crash_before_ready() {
    let e = env();
    e.set_mode("exit73");
    let t0 = Instant::now();
    let err = e.m().spawn(e.spawn_cfg()).await.unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, SandboxError::Backend { .. }), "{msg}");
    assert!(msg.contains("VM exited during boot"), "{msg}");
    assert!(
        msg.contains("exit code 73 (libkrun configuration call failed)"),
        "{msg}"
    );
    assert!(
        msg.contains("vmm.log") && msg.contains("krun_set_vm_config(ctx 0"),
        "vmm.log tail included: {msg}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(3),
        "crash watch, not the boot timeout"
    );
    assert!(gone(&e.helper()).await);
    assert!(e.run_dirs().is_empty());
    assert!(e.leases_released().await);
}

/// A helper that never listens hits the boot deadline, is killed, and leaves nothing behind.
async fn readiness_timeout_when_helper_hangs() {
    let e = env_with(|c| c.boot_timeout = Duration::from_millis(400));
    e.set_mode("hang");
    let t0 = Instant::now();
    let err = e.m().spawn(e.spawn_cfg()).await.unwrap_err();
    let took = t0.elapsed();
    let msg = err.to_string();
    assert!(matches!(err, SandboxError::Timeout { .. }), "{msg}");
    assert!(
        msg.contains("guest control socket") && msg.contains("attempt"),
        "{msg}"
    );
    assert!(
        took >= Duration::from_millis(400) && took < Duration::from_secs(4),
        "{took:?}"
    );
    assert!(gone(&e.helper()).await, "hung helper killed");
    assert!(e.run_dirs().is_empty());
    assert!(e.leases_released().await);
}

/// libkrun accepts and closes host connections until the guest listens: the readiness probe
/// retries past that, and times out (with the probe error) if it never ends.
async fn accept_then_close() {
    let e = env();
    e.set_mode("flaky_accept");
    let sb = e.m().spawn(e.spawn_cfg()).await.unwrap();
    let r: BashResult = sb.call(bash("ok")).await.unwrap();
    assert_eq!(r.stdout, "ok");
    sb.destroy().await.unwrap();
    assert!(e.leases_released().await);

    let e = env_with(|c| c.boot_timeout = Duration::from_millis(400));
    e.set_mode("accept_close");
    let err = e.m().spawn(e.spawn_cfg()).await.unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, SandboxError::Timeout { .. }), "{msg}");
    assert!(msg.contains("probe"), "{msg}");
    assert!(gone(&e.helper()).await);
    assert!(e.run_dirs().is_empty());
    assert!(e.leases_released().await);
}

/// `init.shutdown` acked but ignored → SIGTERM stops the helper.
async fn shutdown_escalates_to_sigterm() {
    let e = env();
    e.set_mode("ignore_shutdown");
    let sb = e.m().spawn(e.spawn_cfg()).await.unwrap();
    let helper = e.helper();
    let t0 = Instant::now();
    sb.destroy().await.unwrap();
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_millis(2000),
        "stopped at the SIGTERM step: {took:?}"
    );
    assert!(e.control_methods().iter().any(|m| m == METHOD_SHUTDOWN));
    assert!(gone(&helper).await);
    assert!(e.run_dirs().is_empty());
    assert!(e.leases_released().await);
}

/// A helper that ignores `init.shutdown`, SIGTERM and the lifeline is SIGKILLed at
/// `kill_after`; destroy returns only once it is gone.
async fn sigterm_escalates_to_sigkill() {
    let e = env_with(|c| {
        c.term_after = Duration::from_millis(200);
        c.kill_after = Duration::from_millis(700);
    });
    e.set_mode("stubborn");
    let sb = e.m().spawn(e.spawn_cfg()).await.unwrap();
    let helper = e.helper();
    let t0 = Instant::now();
    sb.destroy().await.unwrap();
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_millis(700),
        "waited for the ladder: {took:?}"
    );
    assert!(
        !helper.is_alive(),
        "destroy returned only after the SIGKILL reap"
    );
    assert!(e.control_methods().iter().any(|m| m == METHOD_SHUTDOWN));
    assert!(e.has_event("sigterm_ignored"), "SIGTERM was tried first");
    assert!(e.run_dirs().is_empty());
    assert!(e.leases_released().await);
}

/// A VM that crashes after readiness: tool calls fail, destroy still cleans up.
async fn crash_after_ready() {
    let e = env();
    e.set_mode("crash_on_signal");
    let sb = e.m().spawn(e.spawn_cfg()).await.unwrap();
    let helper = e.helper();
    fs::write(e.run_dir().join(sb.id().to_string()).join("crash-now"), "").unwrap();
    assert!(gone(&helper).await);
    assert!(sb.call::<BashResult>(bash("after crash")).await.is_err());
    sb.destroy().await.unwrap();
    assert!(e.run_dirs().is_empty());
    assert!(e.leases_released().await);
}

/// The backend handle is dropped without `destroy`: the helper is killed (SIGKILL + lifeline
/// EOF) and the run dir removed.
async fn drop_without_destroy() {
    let e = env();
    let handle = e.backend.spawn(&bare_spec()).await.unwrap();
    let dir = e.run_dir().join(handle.id().to_string());
    let helper = e.helper();
    assert!(helper.is_alive() && dir.is_dir());
    drop(handle);
    assert!(gone(&helper).await, "helper killed on drop");
    assert!(!dir.exists(), "run dir removed on drop");
}

/// Dropping the manager (and every sandbox handle) tears sandboxes down and releases leases.
async fn manager_drop_releases_leases() {
    let mut e = env();
    let sb = e.m().spawn(e.spawn_cfg()).await.unwrap();
    let helper = e.helper();
    assert_eq!(e.state("docs"), WorkspaceState::LeasedRo { holders: 1 });
    drop(sb);
    drop(e.manager.take());
    assert!(gone(&helper).await, "helper stopped");
    assert!(e.leases_released().await, "leases released on drop");
    // Teardown removes the run dir before the leases go.
    assert!(e.run_dirs().is_empty());
}

/// The owner dies without any cleanup: EOF on the lifeline makes the helper kill itself; the
/// reaper then removes the dead owner's run dir.
async fn lifeline_kills_orphaned_helper() {
    let e = env();
    let dir = run_owner(&e);
    let meta = RunMeta::load(&dir).unwrap();
    let helper = meta.helper.clone().unwrap();
    assert!(!meta.owner.is_alive(), "owner exited");
    assert!(gone(&helper).await, "helper exited on lifeline EOF");
    assert!(e.has_event("lifeline_eof"));
    let report = e.backend.reap_orphans().unwrap();
    assert_eq!(report.run_dirs, std::slice::from_ref(&dir));
    assert!(report.killed.is_empty(), "nothing left to kill");
    assert!(!dir.exists());
}

/// A helper that survived its dead owner (no lifeline) is SIGKILLed by the reaper; a live
/// owner's sandbox (ours) is left alone.
async fn reaper_kills_dead_owners_helper() {
    let e = env();
    e.set_mode("ignore_lifeline");
    let orphan = run_owner(&e);
    let helper = RunMeta::load(&orphan).unwrap().helper.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(helper.is_alive(), "no lifeline: the orphan keeps running");

    let sb = e
        .m()
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap();
    let mine = e.run_dir().join(sb.id().to_string());

    let report = e.backend.reap_orphans().unwrap();
    assert_eq!(report.killed, [helper.pid]);
    assert_eq!(report.run_dirs, std::slice::from_ref(&orphan));
    assert!(gone(&helper).await);
    assert!(!orphan.exists());

    assert!(mine.is_dir(), "live owner's run dir kept");
    let r: BashResult = sb.call(bash("still here")).await.unwrap();
    assert_eq!(r.stdout, "still here");
    sb.destroy().await.unwrap();
    assert!(e.run_dirs().is_empty());
}

/// `unrestricted` needs the explicit TSI opt-in; `egress_restricted` is never enforceable.
/// Refusals happen before anything is launched.
async fn network_policies() {
    let e = env();
    for net in [
        NetworkConfig::unrestricted(),
        NetworkConfig::egress_restricted(["example.com"]),
    ] {
        let err = e.m().spawn(e.spawn_cfg().network(net)).await.unwrap_err();
        assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");
    }
    assert!(e.starts().is_empty(), "no helper launched");
    assert!(e.run_dirs().is_empty());
    assert!(e.leases_released().await);

    let e = env_with(|c| c.allow_tsi_unrestricted = true);
    let sb = e
        .m()
        .spawn(SandboxConfig::from_template("base").network(NetworkConfig::unrestricted()))
        .await
        .unwrap();
    assert_eq!(e.config().network, HelperNetwork::TsiUnrestricted);
    assert!(e.config().netstack.is_none());
    let init = e.init_config();
    assert!(init.network && !init.dns.is_empty());
    sb.destroy().await.unwrap();

    // With the pinned netstack installed, `unrestricted` is the userspace NIC: the helper
    // gets the pinned binary and the host's own addresses to deny; `disabled` stays NIC-less.
    let ns_bin = PathBuf::from("/usr/bin/true");
    let ns = sandcrab_backend_libkrun::NetstackBin {
        kind: NetstackKind::Passt,
        binary: sandcrab_backend_libkrun::helper_proto::PinnedFile {
            sha256: sandcrab_backend_libkrun::artifacts::sha256_file(&ns_bin).unwrap(),
            path: ns_bin,
            stamp: None,
        },
        avx2: None,
    };
    let e = env_libs(|_| {}, |l| l.netstack = Some(ns.clone()));
    let sb = e
        .m()
        .spawn(SandboxConfig::from_template("base").network(NetworkConfig::unrestricted()))
        .await
        .unwrap();
    let cfg = e.config();
    assert_eq!(cfg.network, HelperNetwork::Userspace);
    let spec = cfg.netstack.as_ref().expect("netstack spec");
    assert_eq!((spec.kind, &spec.binary), (ns.kind, &ns.binary));
    assert_eq!(
        spec.deny,
        sandcrab_backend_libkrun::netstack::host_prefixes().unwrap()
    );
    let init = e.init_config();
    assert!(init.network);
    assert_eq!(
        init.dns,
        ["192.168.127.1".parse::<std::net::IpAddr>().unwrap()]
    );
    sb.destroy().await.unwrap();
    let e = env_libs(|_| {}, |l| l.netstack = Some(ns.clone()));
    let sb = e.m().spawn(e.spawn_cfg()).await.unwrap();
    assert_eq!(e.config().network, HelperNetwork::Disabled);
    assert!(e.config().netstack.is_none());
    sb.destroy().await.unwrap();
}

/// Loopback echo server on the test host (the fake guest's "loopback": the fake helper's
/// `MockCrabd` dials host `127.0.0.1` for `tunnel.open`). Echoes until EOF, then half-closes.
async fn echo_server() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.into_split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
                let _ = w.shutdown().await;
            });
        }
    });
    port
}

/// A loopback port nothing listens on.
async fn closed_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

/// Send `data` and read back as many bytes. The client does not half-close: libkrun does not
/// forward a client's `SHUT_WR` into the tunnel (`ports::HOST_HALF_CLOSE`).
async fn echo_round_trip(addr: std::net::SocketAddr, data: &[u8]) -> Vec<u8> {
    let s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut r, mut w) = s.into_split();
    let data = data.to_vec();
    let len = data.len();
    let writer = tokio::spawn(async move {
        w.write_all(&data).await.unwrap();
        w
    });
    let mut back = vec![0u8; len];
    tokio::time::timeout(Duration::from_secs(10), r.read_exact(&mut back))
        .await
        .expect("echo within 10 s")
        .unwrap();
    drop(writer.await.unwrap());
    back
}

/// Wait until `addr` refuses connections (listener closed).
async fn port_closed(addr: std::net::SocketAddr) -> bool {
    for _ in 0..300 {
        if tokio::net::TcpStream::connect(addr).await.is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Published ports: one `127.0.0.1` listener per guest port, each connection a `tunnel.open`
/// over `v.sock`. Works with network `disabled`; a port nothing listens on refuses per
/// connection, not the spawn; destroy closes the listeners.
async fn published_ports_forward_over_tunnels() {
    let e = env();
    let (open, closed) = (echo_server().await, closed_port().await);
    let sb = e
        .m()
        .spawn(e.spawn_cfg().port(open).port(closed))
        .await
        .unwrap();
    let ports = sb.published_ports().to_vec();
    assert_eq!(
        ports.iter().map(|p| p.guest).collect::<Vec<_>>(),
        [open, closed]
    );
    for p in &ports {
        assert_eq!(p.host.ip(), std::net::Ipv4Addr::LOCALHOST, "{p:?}");
        assert_ne!(p.host.port(), 0);
    }
    assert!(e.config().network == HelperNetwork::Disabled);

    // 1 MiB each way, half-close propagated end to end.
    let data: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    assert!(echo_round_trip(ports[0].host, &data).await == data);
    // Concurrent tunnels.
    let mut tasks = Vec::new();
    for i in 0..8u8 {
        let addr = ports[0].host;
        tasks.push(tokio::spawn(async move {
            let msg = vec![i; 4096 + usize::from(i)];
            assert!(echo_round_trip(addr, &msg).await == msg, "tunnel {i}");
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    // Nothing listening: the connection is accepted, then closed without data (-32004).
    let mut s = tokio::net::TcpStream::connect(ports[1].host).await.unwrap();
    let mut buf = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0);
    // ... and the sandbox is still fine.
    let r: BashResult = sb.call(bash("still here")).await.unwrap();
    assert_eq!(r.stdout, "still here");

    sb.destroy().await.unwrap();
    for p in &ports {
        assert!(port_closed(p.host).await, "{p:?} open after destroy");
    }
    assert!(e.run_dirs().is_empty());
    assert!(e.leases_released().await);

    // Invalid lists are refused before anything is launched.
    let e = env();
    for cfg in [e.spawn_cfg().port(0), e.spawn_cfg().port(open).port(open)] {
        let err = e.m().spawn(cfg).await.unwrap_err();
        assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
    }
    assert!(e.starts().is_empty(), "no helper launched");
    assert!(e.run_dirs().is_empty());
}

/// The forwards die with the VM: a crash closes them at once, and so does dropping the
/// backend handle without `destroy`.
async fn published_ports_close_on_crash_and_drop() {
    let open = echo_server().await;

    let e = env();
    e.set_mode("crash_on_signal");
    let sb = e.m().spawn(e.spawn_cfg().port(open)).await.unwrap();
    let addr = sb.published_ports()[0].host;
    assert_eq!(echo_round_trip(addr, b"before").await, b"before");
    let helper = e.helper();
    fs::write(e.run_dir().join(sb.id().to_string()).join("crash-now"), "").unwrap();
    assert!(gone(&helper).await);
    assert!(
        port_closed(addr).await,
        "listener open after the VM crashed"
    );
    sb.destroy().await.unwrap();
    assert!(e.run_dirs().is_empty());

    let e = env();
    let mut spec = bare_spec();
    spec.ports = vec![sandcrab_core::PortSpec { guest: open }];
    let handle = e.backend.spawn(&spec).await.unwrap();
    let addr = handle.published_ports()[0].host;
    assert_eq!(echo_round_trip(addr, b"x").await, b"x");
    drop(handle);
    assert!(port_closed(addr).await, "listener open after drop");
}

// ---------------------------------------------------------------------------------------
// Harness.
// ---------------------------------------------------------------------------------------

type TestFn = fn() -> Pin<Box<dyn Future<Output = ()> + Send>>;

fn harness() -> ExitCode {
    let filter: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let tests: &[(&str, TestFn)] = &[
        ("full_lifecycle", || Box::pin(full_lifecycle())),
        ("crash_before_ready", || Box::pin(crash_before_ready())),
        ("readiness_timeout_when_helper_hangs", || {
            Box::pin(readiness_timeout_when_helper_hangs())
        }),
        ("accept_then_close", || Box::pin(accept_then_close())),
        ("shutdown_escalates_to_sigterm", || {
            Box::pin(shutdown_escalates_to_sigterm())
        }),
        ("sigterm_escalates_to_sigkill", || {
            Box::pin(sigterm_escalates_to_sigkill())
        }),
        ("crash_after_ready", || Box::pin(crash_after_ready())),
        ("drop_without_destroy", || Box::pin(drop_without_destroy())),
        ("manager_drop_releases_leases", || {
            Box::pin(manager_drop_releases_leases())
        }),
        ("lifeline_kills_orphaned_helper", || {
            Box::pin(lifeline_kills_orphaned_helper())
        }),
        ("reaper_kills_dead_owners_helper", || {
            Box::pin(reaper_kills_dead_owners_helper())
        }),
        ("network_policies", || Box::pin(network_policies())),
        ("published_ports_forward_over_tunnels", || {
            Box::pin(published_ports_forward_over_tunnels())
        }),
        ("published_ports_close_on_crash_and_drop", || {
            Box::pin(published_ports_close_on_crash_and_drop())
        }),
    ];
    // Linux builds every scratch disk with mke2fs (macOS uses the seeded blank image).
    if cfg!(not(target_os = "macos")) && Command::new("mke2fs").arg("-V").output().is_err() {
        println!(
            "fake_helper: mke2fs not installed; skipping {} tests",
            tests.len()
        );
        return ExitCode::SUCCESS;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut failed = 0;
    let mut ran = 0;
    for (name, f) in tests {
        if !filter.is_empty() && !filter.iter().any(|p| name.contains(p.as_str())) {
            continue;
        }
        ran += 1;
        let t0 = Instant::now();
        match rt.block_on(async { tokio::spawn(f()).await }) {
            Ok(()) => println!("test {name} ... ok ({:?})", t0.elapsed()),
            Err(e) => {
                failed += 1;
                println!("test {name} ... FAILED: {e}");
            }
        }
    }
    println!(
        "\ntest result: {}. {} passed; {failed} failed",
        if failed == 0 { "ok" } else { "FAILED" },
        ran - failed
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
