//! Full spawn → Firecracker API → vsock → `init.*` → tools → destroy path of the real
//! `FirecrackerBackend`, against a **fake `firecracker`**. No KVM needed.
//!
//! This test binary has its own `main` (`harness = false`): when started with
//! `--api-sock <path>` it *is* the fake VMM, and the backend under test is configured to
//! launch this very executable as its `firecracker` binary. The fake:
//! - serves the Firecracker API on `--api-sock` (HTTP/1.1, keep-alive), recording every
//!   request to `<run_dir>/fake-log/<id>.jsonl`, rejecting drives whose `path_on_host` does
//!   not exist (400 + `fault_message`, like Firecracker);
//! - on `InstanceStart`, listens on the configured vsock UDS and speaks the
//!   `CONNECT <port>` / `OK` handshake; port 1 is a fake `crabd init` control server
//!   (`init.configure` / `init.export` / `init.shutdown` / `ping`), port 1024 is core's
//!   `MockCrabd` (only after `init.configure`, like the real guest);
//! - `init.export` dumps the drive's ext4 image with `debugfs rdump` (when available),
//!   "edits" it like a guest would (adds `guest-wrote.txt`, deletes `delete-me.txt`) and
//!   streams it back as a tar;
//! - `init.shutdown` / `SendCtrlAltDel` exit the process (the VM "powers off").
//!
//! Behaviour switches come from `<run_dir>/fake-mode`: `bad_drive`, `hang` (ignores
//! shutdown and Ctrl-Alt-Del), `no_guest` (vsock never answers), `crash_export`
//! (truncated archive), `slow_export` (one archive byte every 20 ms).
//!
//! The real-KVM equivalents live in `tests/firecracker_kvm.rs` (`#[ignore]`, PENDING).

use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, ExitCode};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sandcrab_backend_firecracker::proc::ProcId;
use sandcrab_backend_firecracker::rundir::RunMeta;
use sandcrab_backend_firecracker::workspace::pack_dir;
use sandcrab_backend_firecracker::{FirecrackerBackend, FirecrackerConfig, RootfsSource};
use sandcrab_core::testing::MockCrabd;
use sandcrab_core::{
    ManagerConfig, MountMode, NetworkConfig, SandboxConfig, SandboxError, SandboxManager,
    TemplateRegistry, WorkspaceRegistry, WorkspaceState,
};
use sandcrab_protocol::init::{InitConfig, METHOD_CONFIGURE, METHOD_EXPORT, METHOD_SHUTDOWN};
use sandcrab_protocol::{
    BashParams, BashResult, HEADER_LEN, METHOD_PING, RpcMessage, RpcResponse, encode_frame,
    parse_header,
};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--api-sock") {
        let sock = PathBuf::from(&args[i + 1]);
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(fake::run(sock));
        return ExitCode::SUCCESS;
    }
    harness()
}

// ---------------------------------------------------------------------------------------
// The fake VMM.
// ---------------------------------------------------------------------------------------

mod fake {
    use super::*;

    #[derive(Default)]
    struct State {
        drives: HashMap<String, PathBuf>,
        vsock: Option<PathBuf>,
        configured: Option<InitConfig>,
    }

    struct Fake {
        mode: String,
        log: PathBuf,
        state: Mutex<State>,
    }

    impl Fake {
        fn record(&self, v: Value) {
            use std::io::Write;
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.log)
                .unwrap();
            writeln!(f, "{v}").unwrap();
        }
    }

    pub async fn run(api_sock: PathBuf) {
        let dir = api_sock.parent().unwrap().to_path_buf();
        let run_dir = dir.parent().unwrap().to_path_buf();
        let id = dir.file_name().unwrap().to_string_lossy().into_owned();
        let mode = fs::read_to_string(run_dir.join("fake-mode")).unwrap_or_default();
        fs::create_dir_all(run_dir.join("fake-log")).unwrap();
        let fake = Arc::new(Fake {
            mode: mode.trim().to_owned(),
            log: run_dir.join("fake-log").join(format!("{id}.jsonl")),
            state: Mutex::default(),
        });
        // Look like a slow-ish VMM: the socket appears a bit after exec.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let listener = UnixListener::bind(&api_sock).unwrap();
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(api_conn(s, Arc::clone(&fake)));
        }
    }

    async fn api_conn(mut s: UnixStream, fake: Arc<Fake>) {
        loop {
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if s.read(&mut b).await.unwrap_or(0) == 0 {
                    return;
                }
                head.push(b[0]);
            }
            let head = String::from_utf8(head).unwrap();
            let mut first = head.lines().next().unwrap().split(' ');
            let method = first.next().unwrap().to_owned();
            let path = first.next().unwrap().to_owned();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .map_or(0, |v| v.trim().parse().unwrap());
            let mut body = vec![0u8; len];
            s.read_exact(&mut body).await.unwrap();
            let body: Value = if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).unwrap()
            };
            fake.record(json!({"method": method, "path": path, "body": body}));
            let (status, resp) = handle_api(&fake, &method, &path, &body).await;
            let resp = resp.map(|v| v.to_string()).unwrap_or_default();
            let msg = format!(
                "HTTP/1.1 {status} \r\nServer: Firecracker API\r\nContent-Length: {}\r\n\r\n{resp}",
                resp.len()
            );
            s.write_all(msg.as_bytes()).await.unwrap();
        }
    }

    async fn handle_api(
        fake: &Arc<Fake>,
        method: &str,
        path: &str,
        body: &Value,
    ) -> (u16, Option<Value>) {
        let fault = |m: &str| (400, Some(json!({"fault_message": m})));
        match (method, path) {
            ("GET", "/") => (
                200,
                Some(
                    json!({"id": "fake", "state": "Not started", "vmm_version": "1.18.0", "app_name": "Firecracker"}),
                ),
            ),
            ("PUT", p) if p.starts_with("/drives/") => {
                if fake.mode == "bad_drive" {
                    return fault("Unable to open drive (fake bad_drive mode)");
                }
                let host = PathBuf::from(body["path_on_host"].as_str().unwrap());
                if !host.is_file() {
                    return fault(&format!("No such file: {}", host.display()));
                }
                fake.state
                    .lock()
                    .unwrap()
                    .drives
                    .insert(body["drive_id"].as_str().unwrap().to_owned(), host);
                (204, None)
            }
            ("PUT", "/vsock") => {
                fake.state.lock().unwrap().vsock =
                    Some(PathBuf::from(body["uds_path"].as_str().unwrap()));
                (204, None)
            }
            ("PUT", "/actions") => match body["action_type"].as_str().unwrap() {
                "InstanceStart" => {
                    let uds = fake.state.lock().unwrap().vsock.clone();
                    let Some(uds) = uds else {
                        return fault("no vsock configured");
                    };
                    let listener = UnixListener::bind(&uds).unwrap();
                    tokio::spawn(vsock_loop(listener, Arc::clone(fake)));
                    (204, None)
                }
                "SendCtrlAltDel" => {
                    if fake.mode != "hang" {
                        power_off();
                    }
                    (204, None)
                }
                _ => (204, None),
            },
            ("PUT", _) => (204, None),
            _ => fault("unsupported"),
        }
    }

    fn power_off() {
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            std::process::exit(0);
        });
    }

    async fn vsock_loop(listener: UnixListener, fake: Arc<Fake>) {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let fake = Arc::clone(&fake);
            tokio::spawn(async move {
                let mut line = Vec::new();
                let mut b = [0u8; 1];
                while s.read(&mut b).await.unwrap_or(0) == 1 && b[0] != b'\n' {
                    line.push(b[0]);
                }
                if fake.mode == "no_guest" {
                    return; // Firecracker closes when no guest listener exists.
                }
                match String::from_utf8_lossy(&line).as_ref() {
                    "CONNECT 1" => {
                        s.write_all(b"OK 1073741824\n").await.unwrap();
                        control(s, fake).await;
                    }
                    "CONNECT 1024" if fake.state.lock().unwrap().configured.is_some() => {
                        s.write_all(b"OK 1073741825\n").await.unwrap();
                        MockCrabd::default().serve(s).await;
                    }
                    _ => {}
                }
            });
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
        w.write_all(&encode_frame(&r).unwrap()).await.unwrap();
    }

    async fn control(mut s: UnixStream, fake: Arc<Fake>) {
        while let Some(p) = read_frame(&mut s).await {
            let Ok(RpcMessage::Request(req)) = RpcMessage::from_slice(&p) else {
                continue;
            };
            let params = req.params.clone().unwrap_or(Value::Null);
            fake.record(json!({"control": req.method, "params": params}));
            match req.method.as_str() {
                METHOD_PING => reply(&mut s, RpcResponse::success(req.id, json!({}))).await,
                METHOD_CONFIGURE => {
                    let cfg: InitConfig = serde_json::from_value(params).unwrap();
                    fake.state.lock().unwrap().configured = Some(cfg);
                    reply(
                        &mut s,
                        RpcResponse::success(req.id, json!({"serve_pid": 2, "boot_ms": 42})),
                    )
                    .await;
                }
                METHOD_EXPORT => {
                    let drive = params["drive_id"].as_str().unwrap().to_owned();
                    let (img, owner, guest_path) = {
                        let st = fake.state.lock().unwrap();
                        let cfg = st.configured.as_ref().unwrap();
                        let m = cfg
                            .mounts
                            .iter()
                            .find(|m| m.drive_id() == Some(drive.as_str()))
                            .unwrap();
                        (
                            st.drives[&drive].clone(),
                            (cfg.user.uid, cfg.user.gid),
                            m.guest_path.clone(),
                        )
                    };
                    let tar = guest_edit_and_pack(&img, owner);
                    reply(
                        &mut s,
                        RpcResponse::success(
                            req.id,
                            json!({"format": "sandcrab-tar-v1", "guest_path": guest_path}),
                        ),
                    )
                    .await;
                    if fake.mode == "slow_export" {
                        for b in &tar {
                            if s.write_all(std::slice::from_ref(b)).await.is_err() {
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    }
                    let n = if fake.mode == "crash_export" {
                        tar.len() / 2
                    } else {
                        tar.len()
                    };
                    let _ = s.write_all(&tar[..n]).await;
                    let _ = s.shutdown().await;
                    return;
                }
                METHOD_SHUTDOWN => {
                    reply(&mut s, RpcResponse::success(req.id, json!({}))).await;
                    if fake.mode != "hang" {
                        power_off();
                    }
                    return;
                }
                _ => {}
            }
        }
    }

    /// What a guest session would leave behind: the image contents (via `debugfs`), plus
    /// `guest-wrote.txt`, minus `delete-me.txt`.
    fn guest_edit_and_pack(img: &Path, owner: (u32, u32)) -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir(&root).unwrap();
        // The VMM runs with an empty environment (no PATH), like the real one.
        let debugfs = ["/usr/sbin/debugfs", "/sbin/debugfs", "/usr/bin/debugfs"]
            .into_iter()
            .find(|p| Path::new(p).is_file())
            .unwrap_or("debugfs");
        let _ = Command::new(debugfs)
            .arg("-R")
            .arg(format!("rdump / {}", root.display()))
            .arg(img)
            .output();
        let _ = fs::remove_dir_all(root.join("lost+found"));
        let _ = fs::remove_file(root.join("delete-me.txt"));
        fs::write(root.join("guest-wrote.txt"), "hello from the guest\n").unwrap();
        let mut out = Vec::new();
        pack_dir(&root, owner, &mut out).unwrap();
        out
    }
}

// ---------------------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------------------

type TestFn = fn() -> Pin<Box<dyn Future<Output = ()> + Send>>;

fn harness() -> ExitCode {
    let filter: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let tests: &[(&str, TestFn)] = &[
        ("spawn_tools_workspaces_sync_back_destroy", || {
            Box::pin(spawn_tools_workspaces_sync_back_destroy())
        }),
        ("unrestricted_network_uses_tap_pool", || {
            Box::pin(unrestricted_network_uses_tap_pool())
        }),
        ("unenforceable_requests_are_refused", || {
            Box::pin(unenforceable_requests_are_refused())
        }),
        ("published_ports_forward_over_vsock", || {
            Box::pin(published_ports_forward_over_vsock())
        }),
        ("api_error_cleans_up", || Box::pin(api_error_cleans_up())),
        ("guest_that_never_boots_times_out", || {
            Box::pin(guest_that_never_boots_times_out())
        }),
        ("hung_vmm_is_killed", || Box::pin(hung_vmm_is_killed())),
        ("failed_export_preserves_image", || {
            Box::pin(failed_export_preserves_image())
        }),
        ("slow_export_hits_deadline", || {
            Box::pin(slow_export_hits_deadline())
        }),
    ];
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    if !have("mke2fs") {
        println!(
            "fake_vmm: mke2fs not installed; skipping {} tests",
            tests.len()
        );
        return ExitCode::SUCCESS;
    }
    let mut failed = 0;
    let mut ran = 0;
    for (name, f) in tests {
        if !filter.is_empty() && !filter.iter().any(|p| name.contains(p.as_str())) {
            continue;
        }
        ran += 1;
        let t0 = Instant::now();
        let res = rt.block_on(async { tokio::spawn(f()).await });
        match res {
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

fn have(tool: &str) -> bool {
    Command::new(tool).arg("-V").output().is_ok()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

struct Env {
    tmp: tempfile::TempDir,
    manager: SandboxManager,
    project: PathBuf,
    docs: PathBuf,
}

impl Env {
    fn run_dir(&self) -> PathBuf {
        self.tmp.path().join("run")
    }

    fn set_mode(&self, mode: &str) {
        fs::create_dir_all(self.run_dir()).unwrap();
        fs::write(self.run_dir().join("fake-mode"), mode).unwrap();
    }

    /// Requests the fake VMM saw, in order (one sandbox per env).
    fn log(&self) -> Vec<Value> {
        let dir = self.run_dir().join("fake-log");
        let mut out = Vec::new();
        for e in fs::read_dir(dir).unwrap().flatten() {
            for line in fs::read_to_string(e.path()).unwrap().lines() {
                out.push(serde_json::from_str(line).unwrap());
            }
        }
        out
    }

    fn api_calls(&self) -> Vec<String> {
        self.log()
            .iter()
            .filter_map(|v| Some(format!("{} {}", v["method"].as_str()?, v["path"].as_str()?)))
            .collect()
    }

    fn control_calls(&self) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|v| v.get("control").is_some())
            .collect()
    }

    /// Run dirs left behind (ignores the fake's own files).
    fn leftover_run_dirs(&self) -> Vec<PathBuf> {
        fs::read_dir(self.run_dir())
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        let n = p.file_name().unwrap().to_string_lossy();
                        n != "fake-log" && n != "fake-mode" && n != "net"
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn env_with(tweak: impl FnOnce(&mut FirecrackerConfig, &Path)) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let rootfs = home.join("rootfs.ext4");
    fs::write(&rootfs, b"not really ext4").unwrap();
    let kernel = home.join("vmlinux");
    fs::write(&kernel, b"not really a kernel").unwrap();
    let mut cfg = FirecrackerConfig::with_home(home, "/unused/crabd");
    cfg.firecracker_bin = std::env::current_exe().unwrap();
    cfg.kernel = kernel;
    cfg.rootfs = RootfsSource::Image(rootfs);
    cfg.run_dir = home.join("run");
    cfg.boot_timeout = Duration::from_secs(5);
    cfg.ctrl_alt_del_after = Duration::from_millis(300);
    cfg.kill_after = Duration::from_millis(600);
    cfg.taps.sysfs_net = home.join("sysfs-net");
    // Fake taps: keep their leases out of the host-wide lock dir the real pool uses. Nested so
    // the parent is created 0700 (the tempdir itself may be group-writable under umask 002).
    cfg.taps.lock_dir = Some(home.join("tap-locks").join("taps"));
    tweak(&mut cfg, home);
    let backend = Arc::new(FirecrackerBackend::new_unchecked(cfg).unwrap());

    let workspaces = WorkspaceRegistry::open(home.join("state")).unwrap();
    let project = home.join("project");
    let docs = home.join("docs");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&docs).unwrap();
    fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(project.join("delete-me.txt"), "bye\n").unwrap();
    fs::write(docs.join("README.md"), "read me\n").unwrap();
    workspaces.register_host_dir("project", &project).unwrap();
    workspaces.register_host_dir("docs", &docs).unwrap();
    workspaces.create("notes").unwrap();
    workspaces
        .put_file(&"notes".into(), "todo.md", b"- [ ] ship\n")
        .unwrap();
    let templates = TemplateRegistry::load(&[repo_root().join("templates")]).unwrap();
    let manager = SandboxManager::new(
        backend,
        workspaces,
        templates,
        ManagerConfig {
            ready_timeout: Duration::from_secs(5),
            ..ManagerConfig::default()
        },
    )
    .unwrap();
    Env {
        tmp,
        manager,
        project,
        docs,
    }
}

fn env() -> Env {
    env_with(|_, _| {})
}

async fn spawn_tools_workspaces_sync_back_destroy() {
    let e = env();
    let reg = e.manager.workspaces().clone();
    let gen_before = reg.get(&"project".into()).unwrap().generation;
    let sb = e
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .mount("project", "/workspace", MountMode::ReadWrite)
                .mount("docs", "/docs", MountMode::ReadOnly)
                .mount("notes", "/notes", MountMode::ReadWrite)
                .env("FOO", "bar")
                .require_hardware_isolation(true),
        )
        .await
        .unwrap();

    // Tools go through MockCrabd on vsock port 1024.
    let r: BashResult = sb
        .call(BashParams {
            command: "echo hi".into(),
            cwd: None,
            env: Default::default(),
            timeout_ms: None,
        })
        .await
        .unwrap();
    assert_eq!(r.stdout, "echo hi");

    // API sequence and bodies.
    let calls = e.api_calls();
    assert_eq!(
        calls,
        [
            "GET /",
            "PUT /machine-config",
            "PUT /boot-source",
            "PUT /drives/rootfs",
            "PUT /drives/scratch",
            "PUT /drives/ws0",
            "PUT /drives/ws1",
            "PUT /drives/ws2",
            "PUT /vsock",
            "PUT /actions",
        ]
    );
    let log = e.log();
    let body = |path: &str| {
        log.iter()
            .find(|v| v["path"] == path)
            .map(|v| v["body"].clone())
            .unwrap()
    };
    assert_eq!(
        body("/machine-config"),
        json!({"vcpu_count": 1, "mem_size_mib": 512, "smt": false})
    );
    let boot_args = body("/boot-source")["boot_args"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        boot_args.contains("init=/sbin/crabd") && !boot_args.contains("ip="),
        "{boot_args}"
    );
    let rootfs = body("/drives/rootfs");
    assert_eq!(
        (
            rootfs["is_root_device"].as_bool(),
            rootfs["is_read_only"].as_bool()
        ),
        (Some(true), Some(true))
    );
    assert_eq!(body("/drives/ws0")["is_read_only"], false);
    let ws1 = body("/drives/ws1");
    assert_eq!(ws1["is_read_only"], true);
    assert!(
        ws1["path_on_host"]
            .as_str()
            .unwrap()
            .contains("/cache/ws-ro/"),
        "ro images come from the shared cache: {ws1}"
    );
    assert_eq!(body("/actions"), json!({"action_type": "InstanceStart"}));

    // init.configure contents.
    let ctl = e.control_calls();
    let cfg: InitConfig = serde_json::from_value(ctl[0]["params"].clone()).unwrap();
    assert_eq!(ctl[0]["control"], "init.configure");
    assert_eq!(cfg.default_cwd, PathBuf::from("/workspace"));
    assert_eq!(cfg.env["FOO"], "bar");
    assert!(!cfg.network && cfg.dns.is_empty());
    assert_eq!(cfg.tools_port, 1024);
    let mounts: Vec<_> = cfg
        .mounts
        .iter()
        .map(|m| {
            (
                m.drive_id().unwrap(),
                m.guest_path.to_str().unwrap(),
                m.read_only,
            )
        })
        .collect();
    assert_eq!(
        mounts,
        [
            ("ws0", "/workspace", false),
            ("ws1", "/docs", true),
            ("ws2", "/notes", false)
        ]
    );
    assert_eq!(cfg.user.uid, rustix::process::getuid().as_raw());

    // Run dir + meta while running.
    let dirs = e.leftover_run_dirs();
    assert_eq!(dirs.len(), 1);
    let meta = RunMeta::load(&dirs[0]).unwrap();
    assert_eq!(meta.guest_boot_ms, Some(42));
    assert_eq!(meta.rw_drives.len(), 2);
    let vmm = meta.vmm.unwrap();
    assert!(vmm.is_alive());
    let mode = fs::metadata(&dirs[0]).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);

    // Destroy: both rw workspaces are exported, then the VMM is shut down.
    let marks = sb.destroy().await.unwrap();
    let names: Vec<_> = marks.custom.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "rootfs_ready",
            "ws_attach_start",
            "ws_attach_done",
            "vmm_started",
            "configured",
            "instance_start",
            "control_connected",
            "init_configured",
            "ws_sync_done",
            "vmm_exited",
            "cleanup_done",
        ]
    );
    assert!(marks.backend_started.is_some() && marks.ready.is_some());
    assert!(marks.backend_started <= marks.ready);
    let ctl: Vec<_> = e
        .control_calls()
        .iter()
        .map(|v| v["control"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ctl,
        [
            "init.configure",
            "init.export",
            "init.export",
            "init.shutdown"
        ]
    );
    assert!(!vmm.is_alive(), "VMM exited");
    assert!(e.leftover_run_dirs().is_empty(), "run dir removed");
    assert!(!e.tmp.path().join("recovery").exists());

    // Sync-back is visible on the host.
    assert_eq!(
        fs::read_to_string(e.project.join("guest-wrote.txt")).unwrap(),
        "hello from the guest\n"
    );
    assert!(!e.project.join("delete-me.txt").exists());
    if have("debugfs") {
        // The image really carried the host tree into the "guest" and back.
        assert_eq!(
            fs::read_to_string(e.project.join("src/main.rs")).unwrap(),
            "fn main() {}\n"
        );
        assert_eq!(
            reg.read_file(&"notes".into(), "todo.md").unwrap(),
            b"- [ ] ship\n"
        );
    }
    assert_eq!(
        reg.read_file(&"notes".into(), "guest-wrote.txt").unwrap(),
        b"hello from the guest\n"
    );
    assert_eq!(
        fs::read_to_string(e.docs.join("README.md")).unwrap(),
        "read me\n"
    );
    assert!(
        !e.docs.join("guest-wrote.txt").exists(),
        "ro mount is never synced"
    );
    let after = reg.get(&"project".into()).unwrap();
    assert!(after.generation > gen_before);
    assert_eq!(after.state, WorkspaceState::Idle);
}

async fn unrestricted_network_uses_tap_pool() {
    let e = env();
    // No taps at all → clear error, nothing left behind.
    let err = e
        .manager
        .spawn(SandboxConfig::from_template("base").network(NetworkConfig::unrestricted()))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("fc-net-setup.sh"), "{err}");
    assert!(e.leftover_run_dirs().is_empty());

    // A user-owned tap in the (fake) sysfs.
    let tap = e.tmp.path().join("sysfs-net/sctap0");
    fs::create_dir_all(&tap).unwrap();
    fs::write(
        tap.join("owner"),
        format!("{}\n", rustix::process::getuid().as_raw()),
    )
    .unwrap();
    let sb = e
        .manager
        .spawn(SandboxConfig::from_template("base").network(NetworkConfig::unrestricted()))
        .await
        .unwrap();
    let log = e.log();
    let net = log
        .iter()
        .find(|v| v["path"] == "/network-interfaces/eth0")
        .expect("NIC configured");
    assert_eq!(
        net["body"],
        json!({"iface_id": "eth0", "host_dev_name": "sctap0", "guest_mac": "06:00:AC:1E:00:02"})
    );
    let boot = log.iter().find(|v| v["path"] == "/boot-source").unwrap();
    assert!(
        boot["body"]["boot_args"]
            .as_str()
            .unwrap()
            .ends_with("ip=172.30.0.2::172.30.0.1:255.255.255.252::eth0:off")
    );
    let cfg: InitConfig = serde_json::from_value(e.control_calls()[0]["params"].clone()).unwrap();
    assert!(cfg.network && !cfg.dns.is_empty());

    // The only tap is taken while the sandbox lives ...
    let err = e
        .manager
        .spawn(SandboxConfig::from_template("base").network(NetworkConfig::unrestricted()))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("in use"), "{err}");
    sb.destroy().await.unwrap();
    // ... and free again after destroy.
    let sb = e
        .manager
        .spawn(SandboxConfig::from_template("base").network(NetworkConfig::unrestricted()))
        .await
        .unwrap();
    sb.destroy().await.unwrap();
    assert!(e.leftover_run_dirs().is_empty());
}

async fn unenforceable_requests_are_refused() {
    let e = env();
    let err = e
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .network(NetworkConfig::egress_restricted(["example.com"])),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");
    for cfg in [
        SandboxConfig::from_template("base").port(0),
        SandboxConfig::from_template("base").port(3000).port(3000),
    ] {
        let err = e.manager.spawn(cfg).await.unwrap_err();
        assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
    }
    assert!(e.leftover_run_dirs().is_empty());
    assert!(
        !e.run_dir().join("fake-log").exists(),
        "no VMM was ever launched"
    );
}

/// Loopback echo server on the test host (the fake guest's "loopback": `MockCrabd` dials host
/// `127.0.0.1` for `tunnel.open`). Echoes until EOF, then half-closes.
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

/// Send `data` and read exactly as many bytes back. No half-close: the Firecracker backend
/// does not forward the client's `SHUT_WR` (its vsock bridge would make it a full close), so
/// an echo-until-EOF exchange never ends there by design.
async fn echo_round_trip(addr: std::net::SocketAddr, data: &[u8]) -> Vec<u8> {
    let s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut r, mut w) = s.into_split();
    let data = data.to_vec();
    let mut back = vec![0u8; data.len()];
    let writer = tokio::spawn(async move {
        w.write_all(&data).await.unwrap();
        w
    });
    tokio::time::timeout(Duration::from_secs(10), r.read_exact(&mut back))
        .await
        .expect("echo within 10 s")
        .unwrap();
    drop(writer.await.unwrap());
    back
}

/// Wait until `addr` refuses connections (listener closed).
async fn port_closed(addr: std::net::SocketAddr) -> bool {
    for _ in 0..500 {
        if tokio::net::TcpStream::connect(addr).await.is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Published ports over vsock `CONNECT 1024` + `tunnel.open`, with network `disabled`: data
/// goes through (1 MiB, length-framed), a port nothing listens on refuses per connection, destroy and a
/// VMM crash close the listeners.
async fn published_ports_forward_over_vsock() {
    let e = env();
    let open = echo_server().await;
    let closed = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let sb = e
        .manager
        .spawn(SandboxConfig::from_template("base").port(open).port(closed))
        .await
        .unwrap();
    let ports = sb.published_ports().to_vec();
    assert_eq!(
        ports.iter().map(|p| p.guest).collect::<Vec<_>>(),
        [open, closed]
    );
    assert!(
        ports
            .iter()
            .all(|p| p.host.ip().is_loopback() && p.host.port() != 0)
    );

    let data: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    assert!(echo_round_trip(ports[0].host, &data).await == data);

    let mut s = tokio::net::TcpStream::connect(ports[1].host).await.unwrap();
    let mut buf = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "nothing listening: closed without data");
    let r: BashResult = sb
        .call(BashParams {
            command: "still here".into(),
            cwd: None,
            env: Default::default(),
            timeout_ms: None,
        })
        .await
        .unwrap();
    assert_eq!(r.stdout, "still here");

    sb.destroy().await.unwrap();
    for p in &ports {
        assert!(port_closed(p.host).await, "{p:?} open after destroy");
    }
    assert!(e.leftover_run_dirs().is_empty());

    // VMM crash (SIGKILL): the listeners close without a destroy.
    let sb = e
        .manager
        .spawn(SandboxConfig::from_template("base").port(open))
        .await
        .unwrap();
    let addr = sb.published_ports()[0].host;
    assert_eq!(echo_round_trip(addr, b"x").await, b"x");
    let vmm = RunMeta::load(&e.leftover_run_dirs()[0])
        .unwrap()
        .vmm
        .unwrap();
    assert!(vmm.kill(true));
    assert!(port_closed(addr).await, "listener open after the VMM died");
    let _ = sb.destroy().await;
    assert!(e.leftover_run_dirs().is_empty());
}

/// Wait until no process with this identity remains (the fake runs as our child).
async fn gone(vmm: ProcId) -> bool {
    for _ in 0..200 {
        if !vmm.is_alive() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// How many fake VMMs were launched in this env (one log file each).
fn launched_vmms(e: &Env) -> usize {
    fs::read_dir(e.run_dir().join("fake-log"))
        .map(|rd| rd.count())
        .unwrap_or(0)
}

async fn api_error_cleans_up() {
    let e = env();
    e.set_mode("bad_drive");
    let err = e
        .manager
        .spawn(SandboxConfig::from_template("base").mount(
            "project",
            "/workspace",
            MountMode::ReadWrite,
        ))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, SandboxError::Backend { .. }), "{msg}");
    assert!(
        msg.contains("drive `rootfs`") && msg.contains("Unable to open drive"),
        "{msg}"
    );
    assert_eq!(launched_vmms(&e), 1);
    assert!(
        e.leftover_run_dirs().is_empty(),
        "run dir removed after a failed spawn"
    );
    assert!(
        !e.tmp.path().join("recovery").exists(),
        "nothing to recover before boot"
    );
    // The rw lease was released (not dirty) and the host dir untouched.
    let ws = e.manager.workspaces().get(&"project".into()).unwrap();
    assert_eq!(ws.state, WorkspaceState::Idle);
    assert!(e.project.join("delete-me.txt").exists());
}

async fn guest_that_never_boots_times_out() {
    let e = env_with(|c, _| c.boot_timeout = Duration::from_millis(400));
    e.set_mode("no_guest");
    let t0 = Instant::now();
    let err = e
        .manager
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("guest did not come up"), "{err}");
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert!(e.leftover_run_dirs().is_empty());
}

async fn hung_vmm_is_killed() {
    let e = env();
    e.set_mode("hang");
    let sb = e
        .manager
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap();
    let vmm = RunMeta::load(&e.leftover_run_dirs()[0])
        .unwrap()
        .vmm
        .unwrap();
    let t0 = Instant::now();
    sb.destroy().await.unwrap();
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_millis(550),
        "waited for the ladder: {took:?}"
    );
    assert!(gone(vmm).await, "SIGKILLed");
    assert!(e.api_calls().contains(&"PUT /actions".to_owned()));
    let cad = e
        .log()
        .iter()
        .any(|v| v["body"]["action_type"] == "SendCtrlAltDel");
    assert!(cad, "SendCtrlAltDel was tried before SIGKILL");
    assert!(e.leftover_run_dirs().is_empty());
}

async fn failed_export_preserves_image() {
    let e = env();
    e.set_mode("crash_export");
    let sb = e
        .manager
        .spawn(SandboxConfig::from_template("base").mount(
            "project",
            "/workspace",
            MountMode::ReadWrite,
        ))
        .await
        .unwrap();
    let id = sb.id();
    let err = sb.destroy().await.unwrap_err().to_string();
    assert!(
        err.contains("sync-back failed") && err.contains("project"),
        "{err}"
    );
    // Host untouched, workspace flagged, guest image kept for recovery.
    assert!(e.project.join("delete-me.txt").exists());
    assert!(!e.project.join("guest-wrote.txt").exists());
    let ws = e.manager.workspaces().get(&"project".into()).unwrap();
    assert_eq!(ws.state, WorkspaceState::DirtySession);
    let rec = e.tmp.path().join("recovery").join(id.to_string());
    assert!(rec.join("ws-0.ext4").is_file(), "image preserved");
    assert!(rec.join("ws-0.json").is_file());
    assert!(e.leftover_run_dirs().is_empty());
    // No staging dirs left anywhere next to the host dir.
    let stray: Vec<_> = fs::read_dir(e.tmp.path())
        .unwrap()
        .flatten()
        .filter(|d| d.file_name().to_string_lossy().contains("staging"))
        .collect();
    assert!(stray.is_empty(), "{stray:?}");
}

async fn slow_export_hits_deadline() {
    let e = env_with(|cfg, _| {
        cfg.export_timeout = Duration::from_millis(800);
        cfg.export_min_bytes_per_sec = u64::MAX; // no size-proportional part
    });
    e.set_mode("slow_export");
    let sb = e
        .manager
        .spawn(SandboxConfig::from_template("base").mount(
            "project",
            "/workspace",
            MountMode::ReadWrite,
        ))
        .await
        .unwrap();
    let id = sb.id();
    let t0 = Instant::now();
    let err = sb.destroy().await.unwrap_err().to_string();
    let took = t0.elapsed();
    // Idle timeout is 30 s and the guest never goes idle; only the overall deadline ends it.
    assert!(took < Duration::from_secs(10), "destroy took {took:?}");
    assert!(
        err.contains("sync-back failed") && err.contains("overall deadline"),
        "{err}"
    );
    assert!(!e.project.join("guest-wrote.txt").exists());
    let rec = e.tmp.path().join("recovery").join(id.to_string());
    assert!(rec.join("ws-0.ext4").is_file(), "image preserved");
    assert!(e.leftover_run_dirs().is_empty());
}
