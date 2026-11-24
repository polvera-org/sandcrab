//! End-to-end tests against **real Firecracker on KVM**: the pinned kernel + firecracker,
//! a rootfs built from `sandcrab/base:0.1.0`, and the real static `crabd` as PID 1.
//!
//! PENDING: requires /dev/kvm (see flow step fc-e2e). None of these has ever run: the
//! build host has no KVM. They are written to mirror `sandcrab-backend-docker`'s e2e suite
//! and to be the acceptance check of step `fc-e2e` on a KVM host. All `#[ignore]`d; run with:
//!
//! ```text
//! sudo usermod -aG kvm $USER                     # then log in again
//! scripts/fetch-artifacts.sh                     # pinned kernel 6.18.62 + firecracker v1.18.0
//! templates/build.sh --pin base                  # sandcrab/base:0.1.0 present + pinned
//! scripts/build-crabd.sh                         # static musl crabd
//! sudo scripts/fc-net-setup.sh --user $USER      # only for the `unrestricted` test
//! cargo test -p sandcrab-backend-firecracker --test firecracker_kvm -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `crabd` comes from `$SANDCRAB_CRABD`, else `target/<arch>-unknown-linux-musl/release/crabd`.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sandcrab_backend_firecracker::{FirecrackerBackend, FirecrackerConfig};
use sandcrab_core::{
    ManagerConfig, MountMode, NetworkConfig, RuntimeBackend, Sandbox, SandboxConfig, SandboxError,
    SandboxManager, TemplateRegistry, WorkspaceRegistry, WorkspaceState,
};
use sandcrab_protocol::{BashParams, BashResult, FileWriteParams, FileWriteResult};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn crabd() -> PathBuf {
    if let Some(p) = std::env::var_os("SANDCRAB_CRABD").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    root().join(format!(
        "target/{}-unknown-linux-musl/release/crabd",
        std::env::consts::ARCH
    ))
}

struct Env {
    tmp: tempfile::TempDir,
    backend: Arc<FirecrackerBackend>,
    manager: SandboxManager,
    project: PathBuf,
    docs: PathBuf,
}

fn env() -> Env {
    // Artifacts + rootfs cache are shared with the real home (slow to build); per-test
    // run / recovery / state dirs are private. Keep run_dir short (UDS path limit).
    let home = sandcrab_backend_firecracker::sandcrab_home().unwrap();
    let tmp = tempfile::Builder::new()
        .prefix("scfc")
        .tempdir_in("/tmp")
        .unwrap();
    let mut cfg = FirecrackerConfig::with_home(&home, crabd());
    cfg.run_dir = tmp.path().join("run");
    cfg.recovery_dir = tmp.path().join("recovery");
    cfg.fetch_pinned_artifacts().unwrap();
    let backend = Arc::new(FirecrackerBackend::new(cfg).unwrap());
    let workspaces = WorkspaceRegistry::open(tmp.path().join("state")).unwrap();
    let project = tmp.path().join("project");
    let docs = tmp.path().join("docs");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(project.join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(docs.join("README.md"), "read me\n").unwrap();
    workspaces.register_host_dir("project", &project).unwrap();
    workspaces.register_host_dir("docs", &docs).unwrap();
    let templates = TemplateRegistry::load(&[root().join("templates")]).unwrap();
    let manager = SandboxManager::new(
        backend.clone(),
        workspaces,
        templates,
        ManagerConfig::default(),
    )
    .unwrap();
    Env {
        tmp,
        backend,
        manager,
        project,
        docs,
    }
}

async fn bash(sb: &Sandbox, cmd: &str) -> BashResult {
    bash_t(sb, cmd, None).await
}

async fn bash_t(sb: &Sandbox, cmd: &str, timeout_ms: Option<u64>) -> BashResult {
    sb.call::<BashResult>(BashParams {
        command: cmd.into(),
        cwd: None,
        env: Default::default(),
        timeout_ms,
    })
    .await
    .unwrap()
}

fn leftovers(env: &Env) -> Vec<PathBuf> {
    std::fs::read_dir(env.tmp.path().join("run"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().is_some_and(|n| n != "net"))
                .collect()
        })
        .unwrap_or_default()
}

fn no_firecracker_left(env: &Env) -> bool {
    let run = env.tmp.path().join("run");
    let me = run.to_string_lossy().into_owned();
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
        .all(|c| !String::from_utf8_lossy(&c).contains(&me))
}

// PENDING: requires /dev/kvm (see flow step fc-e2e)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "PENDING: requires /dev/kvm, pinned artifacts, sandcrab/base:0.1.0 and a static crabd (flow step fc-e2e)"]
async fn base_boot_hello_bash_and_destroy() {
    let env = env();
    let caps = env.backend.capabilities();
    assert!(caps.hardware_isolated && !caps.multi_tenant_safe && !caps.supports_host_dir_mounts);
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base").require_hardware_isolation(true))
        .await
        .unwrap();
    assert_eq!(
        sb.hello().protocol_version,
        sandcrab_protocol::PROTOCOL_VERSION
    );
    let r = bash(&sb, "echo hello; uname -r; cat /proc/1/comm").await;
    assert_eq!(r.exit_code, Some(0));
    let lines: Vec<_> = r.stdout.lines().collect();
    assert_eq!(lines[0], "hello");
    assert!(lines[1].starts_with("6.18"), "guest kernel: {}", lines[1]);
    assert_eq!(lines[2], "crabd", "crabd is PID 1");

    // Runs as the host uid:gid by default; the root fs is an overlay on the scratch drive.
    let r = bash(&sb, "echo $(id -u):$(id -g); findmnt -no FSTYPE /").await;
    let host = format!(
        "{}:{}",
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw()
    );
    assert!(r.stdout.starts_with(&host), "{}", r.stdout);
    assert!(r.stdout.contains("overlay"), "{}", r.stdout);

    // bash timeout kills the whole process group.
    let started = Instant::now();
    let r = bash_t(&sb, "sleep 300 & sleep 301 & wait", Some(500)).await;
    assert!(r.timed_out, "{r:?}");
    assert!(started.elapsed() < Duration::from_secs(10));

    // network `disabled` → only loopback.
    let r = bash(&sb, "ls /sys/class/net").await;
    assert_eq!(r.stdout.split_whitespace().collect::<Vec<_>>(), ["lo"]);

    let marks = sb.destroy().await.unwrap();
    for phase in [
        "vmm_started",
        "instance_start",
        "control_connected",
        "vmm_exited",
    ] {
        assert!(
            marks.custom.iter().any(|(n, _)| n == phase),
            "missing {phase}"
        );
    }
    println!(
        "spawn_ready={:?} teardown={:?}",
        marks.spawn_ready(),
        marks.teardown()
    );
    assert!(leftovers(&env).is_empty(), "run dir removed");
    assert!(no_firecracker_left(&env), "VMM exited");
}

// PENDING: requires /dev/kvm (see flow step fc-e2e)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "PENDING: requires /dev/kvm, pinned artifacts, sandcrab/base:0.1.0 and a static crabd (flow step fc-e2e)"]
async fn rw_and_ro_workspaces_with_sync_back() {
    let env = env();
    let sb = env
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .mount("project", "/workspace", MountMode::ReadWrite)
                .mount("docs", "/ref/docs", MountMode::ReadOnly),
        )
        .await
        .unwrap();

    // The host tree was copied in, owned by the guest user.
    let r = bash(&sb, "cat main.rs; stat -c %u main.rs; pwd").await;
    assert!(r.stdout.starts_with("fn main() {}\n"), "{}", r.stdout);
    assert!(r.stdout.trim_end().ends_with("/workspace"));

    let w = sb
        .call::<FileWriteResult>(FileWriteParams {
            path: "out/hello.txt".into(),
            content: "from the crab\n".into(),
            append: false,
        })
        .await
        .unwrap();
    assert_eq!(w.path, "/workspace/out/hello.txt");
    bash(&sb, "rm main.rs && ln -s /etc/passwd link").await;
    // Not live: the host sees guest writes only after sync-back.
    assert!(!env.project.join("out/hello.txt").exists());

    // ro workspace: EROFS from the guest kernel (drive is also read-only in the VMM).
    let r = bash(&sb, "touch /ref/docs/x.txt 2>&1; cat /ref/docs/README.md").await;
    assert!(r.stdout.contains("Read-only file system"), "{}", r.stdout);
    assert!(r.stdout.contains("read me"));

    let gen_before = env
        .manager
        .workspaces()
        .get(&"project".into())
        .unwrap()
        .generation;
    sb.destroy().await.unwrap();

    let host_file = env.project.join("out/hello.txt");
    assert_eq!(
        std::fs::read_to_string(&host_file).unwrap(),
        "from the crab\n"
    );
    assert_eq!(
        std::fs::metadata(&host_file).unwrap().uid(),
        rustix::process::getuid().as_raw(),
        "synced files belong to the host user"
    );
    assert!(
        !env.project.join("main.rs").exists(),
        "guest deletion applied"
    );
    assert_eq!(
        std::fs::read_link(env.project.join("link")).unwrap(),
        Path::new("/etc/passwd"),
        "symlink stored, never followed"
    );
    assert!(!env.docs.join("x.txt").exists());
    let ws = env.manager.workspaces().get(&"project".into()).unwrap();
    assert!(ws.generation > gen_before);
    assert_eq!(ws.state, WorkspaceState::Idle);

    // Host mutation between turns is visible on the next spawn.
    std::fs::write(env.project.join("host.txt"), "from host").unwrap();
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base").mount(
            "project",
            "/workspace",
            MountMode::ReadWrite,
        ))
        .await
        .unwrap();
    assert_eq!(bash(&sb, "cat host.txt").await.stdout, "from host");
    sb.destroy().await.unwrap();
    assert!(leftovers(&env).is_empty());
    assert!(no_firecracker_left(&env));
}

// PENDING: requires /dev/kvm (see flow step fc-e2e)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "PENDING: requires /dev/kvm and a one-time `sudo scripts/fc-net-setup.sh` (flow step fc-e2e)"]
async fn unrestricted_network_reaches_the_internet() {
    let env = env();
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base").network(NetworkConfig::unrestricted()))
        .await
        .unwrap();
    // The templates ship no iproute2: read the guest address from procfs.
    let r = bash(
        &sb,
        "grep -o '172\\.30\\.[0-9]*\\.2' /proc/net/fib_trie | head -n1; getent hosts example.com",
    )
    .await;
    assert!(r.stdout.contains("172.30."), "{r:?}");
    assert_eq!(r.exit_code, Some(0), "DNS through NAT: {r:?}");
    // Real egress (the guest is IPv4-only; the pool's NAT is IPv4).
    let r = bash(
        &sb,
        "curl -4 -sS -o /dev/null -m 15 -w '%{http_code}' https://example.com",
    )
    .await;
    assert_eq!(r.exit_code, Some(0), "HTTPS through NAT: {r:?}");
    assert!(r.stdout.starts_with(['2', '3']), "{r:?}");
    // guest → link-local / cloud metadata is dropped (private ranges, fc-net-setup.sh).
    let r = bash(
        &sb,
        "timeout 3 bash -c '</dev/tcp/169.254.169.254/80' 2>&1; echo rc=$?",
    )
    .await;
    assert!(!r.stdout.contains("rc=0"), "{}", r.stdout);
    // guest → host is dropped by the `inet sandcrab` table.
    let r = bash(
        &sb,
        "timeout 3 bash -c '</dev/tcp/172.30.0.1/22' 2>&1; echo rc=$?",
    )
    .await;
    assert!(!r.stdout.contains("rc=0"), "{}", r.stdout);
    sb.destroy().await.unwrap();
    assert!(leftovers(&env).is_empty());
}

// PENDING: requires /dev/kvm (see flow step fc-e2e)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "PENDING: requires /dev/kvm, pinned artifacts and a static crabd (flow step fc-e2e)"]
async fn policy_rejections_leave_nothing_behind() {
    let env = env();
    let err = env
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .network(NetworkConfig::egress_restricted(["example.com"])),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");
    let err = env
        .manager
        .spawn(SandboxConfig::from_template("base").port(8080))
        .await
        .unwrap_err();
    assert!(matches!(err, SandboxError::Unimplemented(_)), "{err}");
    assert!(leftovers(&env).is_empty());
    assert!(no_firecracker_left(&env));
}
