//! End-to-end tests against **real libkrun** (Hypervisor.framework on Apple Silicon macOS,
//! KVM on Linux): the signed `sandcrab-krun` helper, the pinned libkrun + libkrunfw, a rootfs
//! built from the pinned template images and the real static `crabd` as PID 1.
//!
//! All `#[ignore]`d. Prerequisites and run:
//!
//! ```text
//! cargo build --release -p sandcrab-backend-libkrun --bin sandcrab-krun
//! sandcrab libkrun install --helper target/release/sandcrab-krun   # libs + signed helper
//! sandcrab template pin base ts-dev                                 # host-local image pins
//! scripts/build-crabd.sh                                            # static musl crabd
//! PATH="$(brew --prefix e2fsprogs)/sbin:$PATH" \
//!   cargo test -p sandcrab-backend-libkrun --test libkrun_vm -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1`: the "no helper left" checks look at every `sandcrab-krun` process of
//! this user. `crabd` comes from `$SANDCRAB_CRABD`, else
//! `$CARGO_TARGET_DIR/<arch>-unknown-linux-musl/release/crabd`, else the repo's `target/`.
//! The helper, libraries and rootfs cache are the real `$SANDCRAB_HOME` ones (slow to build);
//! run dirs and workspaces are private to each test.
//!
//! The `userspace_*` network tests also need the pinned netstack (`sandcrab libkrun install`:
//! gvproxy on macOS, passt on Linux via `scripts/fetch-libkrun.sh`) and internet access.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sandcrab_backend_libkrun::{KrunLibs, helper_proto::PinnedFile};
use sandcrab_backend_libkrun::{LibkrunBackend, LibkrunConfig, artifacts::Artifacts};
use sandcrab_core::{
    BoxedStream, ForwardTarget, ManagerConfig, MountMode, MountPolicy, MountRoot, NetworkConfig,
    NprocScope, ProcessEvent, ProcessOutput, ResourceOverrides, RuntimeBackend, Sandbox,
    SandboxConfig, SandboxError, SandboxManager, TemplateRegistry, TimingMarks, UserSpec,
    WorkspaceRegistry,
};
use sandcrab_protocol::{
    BashParams, BashResult, ExecLimits, FileWriteParams, FileWriteResult, ListDirParams,
    ListDirResult, OutputCap, Overflow, ProcessExit, ProcessOpenParams, ProcessPollParams,
    ProcessPollResult, ProcessStartParams, ProcessStartResult, ProtocolError, StderrMode,
    StdinMode,
};

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
    sandcrab_core::crabd_build_path(&root())
}

struct Env {
    tmp: tempfile::TempDir,
    backend: Arc<LibkrunBackend>,
    manager: SandboxManager,
    project: PathBuf,
    docs: PathBuf,
}

fn config(tmp: &Path, tsi: bool, netstack: bool) -> (LibkrunConfig, KrunLibs) {
    config_with(tmp, tsi, netstack, |_| {})
}

/// [`config`], with `pre` adjusting the paths before the artifacts are resolved.
fn config_with(
    tmp: &Path,
    tsi: bool,
    netstack: bool,
    pre: impl FnOnce(&mut LibkrunConfig),
) -> (LibkrunConfig, KrunLibs) {
    let home = sandcrab_rootfs::sandcrab_home().unwrap();
    let mut cfg = LibkrunConfig::with_home(&home, crabd());
    cfg.run_dir = tmp.join("run");
    cfg.allow_tsi_unrestricted = tsi;
    pre(&mut cfg);
    let a =
        Artifacts::resolve(&cfg).expect("pinned libkrun artifacts (`sandcrab libkrun install`)");
    let pin = |p: sandcrab_backend_libkrun::artifacts::PinnedPath| -> PinnedFile { p.into() };
    let netstack = netstack.then(|| {
        sandcrab_backend_libkrun::netstack_pins::resolve_config(&cfg)
            .expect("netstack install verifies")
            .expect("pinned netstack (`sandcrab libkrun install`)")
    });
    let libs = KrunLibs {
        libkrun: pin(a.libkrun),
        libkrunfw: pin(a.libkrunfw),
        netstack,
    };
    (cfg, libs)
}

fn env() -> Env {
    env_with(false)
}

fn env_with(tsi: bool) -> Env {
    env_cfg(tsi, |_| {})
}

fn env_cfg(tsi: bool, tweak: impl FnOnce(&mut LibkrunConfig)) -> Env {
    env_full(tsi, false, tweak)
}

/// With the pinned userspace netstack (network `unrestricted` → `userspace`).
fn env_net() -> Env {
    env_full(false, true, |_| {})
}

fn env_full(tsi: bool, netstack: bool, tweak: impl FnOnce(&mut LibkrunConfig)) -> Env {
    // Keep the run dir short: socket paths must fit in 104 bytes on macOS.
    let tmp = tempfile::Builder::new()
        .prefix("sclk")
        .tempdir_in("/tmp")
        .unwrap();
    let (mut cfg, libs) = config(tmp.path(), tsi, netstack);
    tweak(&mut cfg);
    env_from(tmp, cfg, libs)
}

fn env_from(tmp: tempfile::TempDir, cfg: LibkrunConfig, libs: KrunLibs) -> Env {
    let backend = Arc::new(LibkrunBackend::new(cfg, libs).unwrap());
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
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// No `sandcrab-krun` process of this user is left (tests run with `--test-threads=1`).
/// Matches the command line, not the name: on Linux libkrun renames the VMM process
/// (`comm` = "libkrun VM").
fn no_helper_left() -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = std::process::Command::new("pgrep")
            .args(["-f", "-U"])
            .arg(rustix::process::getuid().as_raw().to_string())
            .arg("(^|/)sandcrab-krun( |$)")
            .output()
            .unwrap();
        if out.stdout.is_empty() {
            return true;
        }
        if Instant::now() > deadline {
            eprintln!("helpers left: {}", String::from_utf8_lossy(&out.stdout));
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn print_marks(what: &str, marks: &TimingMarks) {
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let mut line = format!(
        "TIMINGS {what}: spawn_ready={:.1}ms teardown={:.1}ms |",
        marks.spawn_ready().map_or(f64::NAN, ms),
        marks.teardown().map_or(f64::NAN, ms)
    );
    for (name, t) in &marks.custom {
        line.push_str(&format!(" {name}={:.1}", ms(*t - marks.spawn_requested)));
    }
    println!("{line}");
}

/// Lists links (`name flags`), non-loopback addresses and non-local routes in the guest.
const NET_STATE: &str = "for i in /sys/class/net/*; do echo \"link $(basename $i) $(cat $i/flags)\"; done; \
     awk '/\\|--/{ip=$2} /host LOCAL/{print \"addr \" ip}' /proc/net/fib_trie | grep -v ' 127\\.'; \
     awk '$6 != \"lo\" {print \"addr6 \" $6}' /proc/net/if_inet6; \
     tail -n +2 /proc/net/route | awk '{print \"route \" $1}'; true";

/// Network `disabled`: loopback only. libkrunfw's kernel always creates `dummy0` (TSI's
/// interface); with TSI off it stays down, without addresses or routes, and the sandbox user
/// has no `CAP_NET_ADMIN` to change that (deleting it in the guest costs ~25 ms of boot).
async fn assert_loopback_only(sb: &Sandbox) {
    let r = bash(sb, NET_STATE).await;
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    for line in r.stdout.lines() {
        let f: Vec<_> = line.split_whitespace().collect();
        match f.as_slice() {
            ["link", "lo", _] => {}
            ["link", name, flags] if name.starts_with("dummy") => {
                let flags = u32::from_str_radix(flags.trim_start_matches("0x"), 16).unwrap();
                assert_eq!(flags & 1, 0, "{name} is up: {}", r.stdout);
            }
            _ => panic!("unexpected network state `{line}`:\n{}", r.stdout),
        }
    }
}

/// The host's primary non-loopback IPv4 (no packet is sent), if it has one.
fn host_lan_ip() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
async fn base_boot_hello_bash_and_destroy() {
    let env = env();
    let caps = env.backend.capabilities();
    assert!(caps.hardware_isolated && !caps.multi_tenant_safe);
    assert!(caps.supports_host_dir_mounts && !caps.supports_snapshots);
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base").require_hardware_isolation(true))
        .await
        .unwrap();
    assert_eq!(
        sb.hello().protocol_version,
        sandcrab_protocol::PROTOCOL_VERSION
    );
    let r = bash(&sb, "echo hello; uname -sm; cat /proc/1/comm").await;
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    let lines: Vec<_> = r.stdout.lines().collect();
    assert_eq!(lines[0], "hello");
    assert!(lines[1].starts_with("Linux"), "guest kernel: {}", lines[1]);
    assert_eq!(lines[2], "crabd", "crabd is PID 1");

    // Runs as the host uid:gid by default (virtio-fs ownership); root is an overlay.
    let r = bash(&sb, "echo $(id -u):$(id -g); findmnt -no FSTYPE /").await;
    let host = format!(
        "{}:{}",
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw()
    );
    assert!(r.stdout.starts_with(&host), "{}", r.stdout);
    assert!(r.stdout.contains("overlay"), "{}", r.stdout);

    // Hardening (guest-hardening steps): no_new_privs, no capabilities, empty bounding set.
    let status = bash(&sb, "cat /proc/self/status").await.stdout;
    let field = |k: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(k))
            .map(str::trim)
            .unwrap_or_else(|| panic!("{k} missing"))
            .to_owned()
    };
    assert_eq!(field("NoNewPrivs:"), "1");
    assert_eq!(u64::from_str_radix(&field("CapEff:"), 16).unwrap(), 0);
    assert_eq!(u64::from_str_radix(&field("CapBnd:"), 16).unwrap(), 0);

    // Orphans re-parented to PID 1 are reaped (no zombies), even when they escaped the
    // tool's process group.
    let r = bash(
        &sb,
        "for i in 1 2 3 4 5; do setsid sh -c 'sleep 0.2 &' ; done; sleep 1.5; \
         ps -eo stat=,ppid=,comm= | awk '$1 ~ /^Z/' | wc -l",
    )
    .await;
    assert_eq!(r.stdout.trim(), "0", "zombies left: {r:?}");

    // bash timeout kills the whole process group.
    let started = Instant::now();
    let r = bash_t(&sb, "sleep 300 & sleep 301 & wait", Some(500)).await;
    assert!(r.timed_out, "{r:?}");
    assert!(started.elapsed() < Duration::from_secs(10));

    // network `disabled` → only loopback (TSI off).
    assert_loopback_only(&sb).await;

    let marks = sb.destroy().await.unwrap();
    for phase in [
        "rootfs_ready",
        "helper_started",
        "control_connected",
        "init_configured",
        "tools_ready",
        "vmm_exited",
    ] {
        assert!(
            marks.custom.iter().any(|(n, _)| n == phase),
            "missing {phase}"
        );
    }
    print_marks("base", &marks);
    assert!(leftovers(&env).is_empty(), "run dir removed");
    assert!(no_helper_left(), "helper exited");
}

/// Warm spawn → ready, several times, to report the breakdown.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
async fn spawn_timing_breakdown() {
    let env = env();
    for i in 0..5 {
        let sb = env
            .manager
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        // The helper's own phase log and the guest's boot time (from `init.configure`).
        let dir = env.tmp.path().join("run").join(sb.id().to_string());
        let log = std::fs::read_to_string(dir.join("vmm.log")).unwrap_or_default();
        let meta = std::fs::read_to_string(dir.join("meta.json")).unwrap_or_default();
        let boot_ms = serde_json::from_str::<serde_json::Value>(&meta)
            .ok()
            .and_then(|m| m["guest_boot_ms"].as_u64());
        let marks = sb.destroy().await.unwrap();
        print_marks(&format!("warm#{i}"), &marks);
        println!("  guest_boot_ms={boot_ms:?}");
        for l in log.lines().filter(|l| l.contains(" ms")) {
            println!("  {l}");
        }
    }
    assert!(leftovers(&env).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
async fn live_rw_and_ro_virtiofs_workspaces() {
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

    let r = bash(&sb, "cat main.rs; stat -c %u main.rs; pwd").await;
    assert!(r.stdout.starts_with("fn main() {}\n"), "{}", r.stdout);
    assert!(r.stdout.trim_end().ends_with("/workspace"));

    // guest → host, live (no sync-back).
    let w = sb
        .call::<FileWriteResult>(FileWriteParams {
            path: "out/hello.txt".into(),
            content: "from the crab\n".into(),
            append: false,
        })
        .await
        .unwrap();
    assert_eq!(w.path, "/workspace/out/hello.txt");
    assert_eq!(
        std::fs::read_to_string(env.project.join("out/hello.txt")).unwrap(),
        "from the crab\n",
        "guest write visible on the host while the sandbox runs"
    );
    bash(&sb, "ln -s /etc/passwd link && rm main.rs").await;
    assert_eq!(
        std::fs::read_link(env.project.join("link")).unwrap(),
        Path::new("/etc/passwd"),
        "guest symlink is a real (inert) host symlink"
    );
    assert!(!env.project.join("main.rs").exists(), "live delete");
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(env.project.join("out/hello.txt"))
                .unwrap()
                .uid(),
            rustix::process::getuid().as_raw(),
            "guest files belong to the host user"
        );
    }

    // host → guest, live, both rw and ro shares.
    std::fs::write(env.project.join("host.txt"), "from host").unwrap();
    std::fs::write(env.docs.join("late.md"), "late doc").unwrap();
    assert_eq!(bash(&sb, "cat host.txt").await.stdout, "from host");
    assert_eq!(bash(&sb, "cat /ref/docs/late.md").await.stdout, "late doc");

    // ro share: EROFS, and a remount cannot lift it.
    let r = bash(&sb, "touch /ref/docs/x.txt 2>&1; cat /ref/docs/README.md").await;
    assert!(r.stdout.contains("Read-only file system"), "{}", r.stdout);
    assert!(r.stdout.contains("read me"));
    let r = bash(
        &sb,
        "mount -o remount,rw /ref/docs 2>&1; echo rc=$?; touch /ref/docs/y.txt 2>&1; \
         findmnt -no OPTIONS /ref/docs",
    )
    .await;
    assert!(
        !r.stdout.contains("rc=0"),
        "remount succeeded: {}",
        r.stdout
    );
    assert!(r.stdout.contains("Read-only file system"), "{}", r.stdout);
    assert!(r.stdout.contains("ro"), "{}", r.stdout);
    assert!(!env.docs.join("x.txt").exists() && !env.docs.join("y.txt").exists());

    sb.destroy().await.unwrap();
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left());
}

/// The helper hands libkrun `/dev/fd/<n>` of the verified root, not the path: swapping the
/// host directory after spawn does not redirect the share (design §8 TOCTOU).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
async fn share_root_is_pinned_by_fd() {
    let env = env();
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base").mount(
            "project",
            "/workspace",
            MountMode::ReadWrite,
        ))
        .await
        .unwrap();
    let moved = env.tmp.path().join("project.orig");
    std::fs::rename(&env.project, &moved).unwrap();
    std::fs::create_dir(&env.project).unwrap();
    std::fs::write(env.project.join("decoy.txt"), "decoy").unwrap();

    let r = bash(&sb, "ls /workspace; echo after > /workspace/after.txt").await;
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    assert!(r.stdout.contains("main.rs"), "{}", r.stdout);
    assert!(
        !r.stdout.contains("decoy"),
        "share followed the path: {}",
        r.stdout
    );
    assert_eq!(
        std::fs::read_to_string(moved.join("after.txt")).unwrap(),
        "after\n"
    );
    assert!(!env.project.join("after.txt").exists());
    sb.destroy().await.unwrap();
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left());
}

/// Two workspaces, one of them holding more files than the host's default soft
/// `RLIMIT_NOFILE` (1024). Linux libkrun's passthrough server keeps one `O_PATH` fd per inode
/// the guest has looked up, until the guest forgets it, so walking the big tree used to exhaust
/// the helper's fd table: the next host `openat` (a guest create) failed with `EMFILE`
/// ("No file descriptors available", the bench W2 cell). The helper raises its soft limit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
async fn two_workspaces_many_files_then_write_into_both() {
    const DIRS: usize = 30;
    const PER_DIR: usize = 100;
    let env = env();
    let bulk = env.tmp.path().join("bulk");
    for d in 0..DIRS {
        let dir = bulk.join(format!("d{d:02}"));
        std::fs::create_dir_all(&dir).unwrap();
        for f in 0..PER_DIR {
            std::fs::write(dir.join(format!("f{f:03}.txt")), "x").unwrap();
        }
    }
    env.manager
        .workspaces()
        .register_host_dir("bulk", &bulk)
        .unwrap();
    let sb = env
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .mount("project", "/workspace", MountMode::ReadWrite)
                .mount("bulk", "/data", MountMode::ReadWrite),
        )
        .await
        .unwrap();

    // What the bench W2 cell does first: crabd list_dir (depth 1) of both shares.
    for path in ["/workspace", "/data"] {
        let l = sb
            .call::<ListDirResult>(ListDirParams {
                path: Some(path.into()),
                depth: Some(1),
                respect_gitignore: false,
            })
            .await;
        assert!(l.is_ok(), "list_dir {path}: {l:?}");
    }
    // Then look up (and keep cached) every inode of the big share: `find -type f` alone
    // reads d_type and never stats a file, so open each one.
    let r = bash(&sb, "find /data -type f -exec cat {} + | wc -c").await;
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    assert_eq!(r.stdout.trim(), (DIRS * PER_DIR).to_string(), "{r:?}");

    for (path, host) in [
        ("/workspace/.bench-sync", env.project.join(".bench-sync")),
        ("/data/.bench-sync", bulk.join(".bench-sync")),
    ] {
        let w = sb
            .call::<FileWriteResult>(FileWriteParams {
                path: path.into(),
                content: "sync".into(),
                append: false,
            })
            .await;
        assert!(w.is_ok(), "write {path}: {w:?}");
        assert_eq!(std::fs::read_to_string(&host).unwrap(), "sync", "{path}");
    }
    // And a fresh file in a new dir of each share, through the shell.
    let r = bash(
        &sb,
        "mkdir -p /workspace/n /data/n && echo a > /workspace/n/a && echo b > /data/n/b",
    )
    .await;
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    assert_eq!(
        std::fs::read_to_string(env.project.join("n/a")).unwrap(),
        "a\n"
    );
    assert_eq!(std::fs::read_to_string(bulk.join("n/b")).unwrap(), "b\n");

    sb.destroy().await.unwrap();
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left());
}

/// TSI stays off with `disabled`: the guest cannot reach the host's loopback (TSI would hijack
/// `127.0.0.1` to the host) or LAN address, and a guest listener is not published on the host.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/ts-dev:0.1.0, static crabd"]
async fn tsi_off_regressions() {
    let env = env();
    let host_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    host_listener.set_nonblocking(true).unwrap();
    let host_port = host_listener.local_addr().unwrap().port();
    let lan = host_lan_ip();
    let lan_listener = lan.map(|ip| {
        let l = TcpListener::bind((ip, 0)).unwrap();
        l.set_nonblocking(true).unwrap();
        l
    });
    let accepted = Arc::new(AtomicUsize::new(0));

    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("ts-dev"))
        .await
        .unwrap();

    // guest → host loopback (the TSI hole) and host LAN.
    let r = bash(
        &sb,
        &format!("timeout 3 bash -c 'echo hi > /dev/tcp/127.0.0.1/{host_port}' 2>&1; echo rc=$?"),
    )
    .await;
    assert!(
        !r.stdout.contains("rc=0"),
        "guest reached host loopback: {r:?}"
    );
    if let (Some(ip), Some(l)) = (lan, &lan_listener) {
        let port = l.local_addr().unwrap().port();
        let r = bash(
            &sb,
            &format!("timeout 3 bash -c 'echo hi > /dev/tcp/{ip}/{port}' 2>&1; echo rc=$?"),
        )
        .await;
        assert!(!r.stdout.contains("rc=0"), "guest reached host LAN: {r:?}");
    } else {
        println!("PROBE no LAN address on this host: LAN checks skipped");
    }
    for l in std::iter::once(&host_listener).chain(lan_listener.as_ref()) {
        while let Ok((mut s, peer)) = l.accept() {
            let mut b = Vec::new();
            let _ = s.read_to_end(&mut b);
            eprintln!("unexpected connection from {peer}: {b:?}");
            accepted.fetch_add(1, Ordering::Relaxed);
        }
    }
    assert_eq!(
        accepted.load(Ordering::Relaxed),
        0,
        "host listener was reached"
    );

    // A guest listener on the wildcard address is not reachable from the host.
    const GUEST_PORT: u16 = 47_123;
    bash(
        &sb,
        &format!(
            "cat > /tmp/l.js <<'EOF'\nrequire('net').createServer(s => s.end('guest')).listen({GUEST_PORT}, '0.0.0.0', () => console.log('LISTENING'));\nEOF"
        ),
    )
    .await;
    let proc = start_and_wait(&sb, "node /tmp/l.js").await;
    let r = bash(
        &sb,
        &format!("timeout 3 bash -c 'cat < /dev/tcp/127.0.0.1/{GUEST_PORT}'"),
    )
    .await;
    assert_eq!(r.stdout, "guest", "guest listener works inside: {r:?}");
    let mut targets = vec![SocketAddr::from((Ipv4Addr::LOCALHOST, GUEST_PORT))];
    if let Some(ip) = lan {
        targets.push(SocketAddr::new(ip, GUEST_PORT));
    }
    for t in targets {
        let r =
            tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(t)).await;
        let reached = matches!(r, Ok(Ok(_)));
        assert!(!reached, "guest listener reachable from the host at {t}");
    }
    drop(proc);

    sb.destroy().await.unwrap();
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, static crabd"]
async fn policy_rejections_leave_nothing_behind() {
    let env = env();
    // `unrestricted` without the TSI opt-in.
    let err = env
        .manager
        .spawn(SandboxConfig::from_template("base").network(NetworkConfig::unrestricted()))
        .await
        .unwrap_err();
    assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");
    assert!(err.to_string().contains("libkrun install"), "{err}");
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
        .spawn(SandboxConfig::from_template("base").port(8080).port(8080))
        .await
        .unwrap_err();
    assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left());
    // The workspace leases were released: a rw spawn of the same workspace works.
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base").mount(
            "project",
            "/workspace",
            MountMode::ReadWrite,
        ))
        .await
        .unwrap();
    sb.destroy().await.unwrap();
    assert!(leftovers(&env).is_empty());
}

/// No netstack (passt / gvproxy) process of this user is left.
/// Matches only this test home's netstack install (other users of the host have their own).
fn netstack_pattern() -> String {
    let home = sandcrab_rootfs::sandcrab_home().unwrap();
    let dir = LibkrunConfig::with_home(&home, crabd())
        .netstack_dir
        .expect("a netstack pin for this host");
    let escaped: String = dir
        .to_string_lossy()
        .chars()
        .flat_map(|c| {
            let esc = "\\.^$*+?()[]{}|".contains(c);
            esc.then_some('\\').into_iter().chain(std::iter::once(c))
        })
        .collect();
    format!("^{escaped}/")
}

fn netstacks_now() -> String {
    let out = std::process::Command::new("pgrep")
        .args(["-f", "-U"])
        .arg(rustix::process::getuid().as_raw().to_string())
        .arg(netstack_pattern())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn no_netstack_left() -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = netstacks_now();
        if out.is_empty() {
            return true;
        }
        if Instant::now() > deadline {
            eprintln!("netstacks left: {out}");
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Nonblocking TCP listener; `None` if the address cannot be bound (e.g. no IPv6).
fn listen(ip: IpAddr) -> Option<TcpListener> {
    let l = TcpListener::bind((ip, 0)).ok()?;
    l.set_nonblocking(true).unwrap();
    Some(l)
}

/// Every connection the listeners accepted so far (and drains them).
fn accepted(listeners: &[&TcpListener]) -> Vec<String> {
    let mut hits = Vec::new();
    for l in listeners {
        while let Ok((mut s, peer)) = l.accept() {
            let mut b = Vec::new();
            let _ = s.read_to_end(&mut b);
            hits.push(format!(
                "{} <- {peer}: {:?}",
                l.local_addr().unwrap(),
                String::from_utf8_lossy(&b)
            ));
        }
    }
    hits
}

/// Network `unrestricted` on the userspace netstack (design §9 Phase B):
/// - the guest has `eth0` (DHCP) and reaches the internet (DNS + TCP 443);
/// - regression: host loopback is unreachable — a listener bound only to 127.0.0.1 (and
///   ::1), probed at every address a netstack maps to the host (gvproxy's NAT alias
///   192.168.127.254, the gateway 192.168.127.1), the host's own LAN address and a 0.0.0.0
///   listener there; the gateway serves nothing but DNS (gvproxy's HTTP API on :80 is
///   filtered); metadata and SMTP are denied;
/// - the netstack dies with the VM.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun + pinned netstack + internet: signed helper, pinned sandcrab/base:0.1.0, static crabd"]
async fn userspace_network_reaches_internet_not_the_host() {
    let env = env_net();
    let lo4 = listen(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
    let lo6 = listen(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST));
    let any = listen(IpAddr::V4(Ipv4Addr::UNSPECIFIED)).unwrap();
    let lan = host_lan_ip();
    let lan_l = lan.and_then(listen);
    let lo4_port = lo4.local_addr().unwrap().port();
    let any_port = any.local_addr().unwrap().port();

    let t = Instant::now();
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base").network(NetworkConfig::unrestricted()))
        .await
        .unwrap();
    println!(
        "TIMING userspace spawn {:.0} ms",
        t.elapsed().as_secs_f64() * 1e3
    );

    // eth0 is up with the DHCP lease, default route via the gateway, DNS at the gateway.
    let r = bash(&sb, &format!("{NET_STATE}; cat /etc/resolv.conf")).await;
    println!("NET_STATE userspace:\n{}", r.stdout);
    assert!(r.stdout.contains("addr 192.168.127.2"), "{}", r.stdout);
    assert!(r.stdout.contains("route eth0"), "{}", r.stdout);
    assert!(
        r.stdout.contains("nameserver 192.168.127.1"),
        "{}",
        r.stdout
    );
    // IPv6: at most the kernel's link-local address; the egress filter drops every IPv6
    // frame, so no global address or route ever appears.
    let v6 = bash(
        &sb,
        "awk '$6 == \"eth0\" && $1 !~ /^fe80/' /proc/net/if_inet6; \
         awk '$10 == \"eth0\" && $1 !~ /^(fe80|ff)/' /proc/net/ipv6_route",
    )
    .await;
    assert_eq!(v6.stdout.trim(), "", "IPv6 beyond link-local on eth0");

    // (a) Internet: DNS + HTTPS.
    let r = bash_t(
        &sb,
        "getent hosts example.com && curl -sS -o /dev/null -w 'HTTP %{http_code}\\n' \
         --max-time 20 https://example.com/; echo rc=$?",
        Some(40_000),
    )
    .await;
    println!("PROBE internet: {}", r.stdout.trim());
    assert!(
        r.stdout.contains("HTTP 200") && r.stdout.contains("rc=0"),
        "{r:?}"
    );

    // (b) Host loopback, the host itself, the gateway's other services, metadata, SMTP.
    let mut probes: Vec<(String, u16)> = Vec::new();
    for ip in ["192.168.127.254", "192.168.127.1", "10.0.2.2", "127.0.0.1"] {
        probes.push((ip.into(), lo4_port));
        probes.push((ip.into(), any_port));
    }
    probes.push(("192.168.127.1".into(), 80)); // gvproxy's API
    probes.push(("192.168.127.1".into(), 22)); // gvproxy SSH forward (disabled anyway)
    probes.push(("169.254.169.254".into(), 80)); // cloud metadata
    if let Some(ip) = lan {
        probes.push((ip.to_string(), any_port));
        if let Some(l) = &lan_l {
            probes.push((ip.to_string(), l.local_addr().unwrap().port()));
        }
    } else {
        println!("PROBE no LAN address on this host: LAN checks skipped");
    }
    let script: String = probes
        .iter()
        .map(|(ip, port)| {
            format!(
                "(timeout 3 bash -c 'echo probe > /dev/tcp/{ip}/{port}' 2>/dev/null \
                 && echo 'OPEN {ip}:{port}' || echo 'closed {ip}:{port}') & "
            )
        })
        .collect::<String>()
        + "wait";
    let r = bash_t(&sb, &script, Some(30_000)).await;
    println!("PROBE host isolation:\n{}", r.stdout.trim());
    // 127.0.0.1 inside the guest is the guest's own loopback: nothing listens there.
    assert!(
        !r.stdout.contains("OPEN"),
        "guest reached a host service:\n{}",
        r.stdout
    );
    assert_eq!(
        r.stdout.lines().filter(|l| l.starts_with("closed")).count(),
        probes.len()
    );
    // DNS on the gateway is the one service it offers.
    let r = bash(
        &sb,
        "timeout 5 getent hosts example.org >/dev/null; echo rc=$?",
    )
    .await;
    assert!(r.stdout.contains("rc=0"), "{r:?}");
    // SMTP submission is denied even to the public internet.
    let r = bash(
        &sb,
        "timeout 5 bash -c 'echo > /dev/tcp/smtp.gmail.com/587' 2>/dev/null; echo rc=$?",
    )
    .await;
    println!("PROBE smtp: {}", r.stdout.trim());
    assert!(!r.stdout.contains("rc=0"), "SMTP reachable: {r:?}");

    let mut ls = vec![&lo4, &any];
    ls.extend(lo6.as_ref());
    ls.extend(lan_l.as_ref());
    let hits = accepted(&ls);
    assert!(hits.is_empty(), "host listeners were reached: {hits:#?}");
    println!(
        "PROBE host listeners untouched: 127.0.0.1:{lo4_port}, 0.0.0.0:{any_port}{}{}",
        lo6.as_ref()
            .map(|l| format!(", {}", l.local_addr().unwrap()))
            .unwrap_or_default(),
        lan_l
            .as_ref()
            .map(|l| format!(", {}", l.local_addr().unwrap()))
            .unwrap_or_default()
    );

    sb.destroy().await.unwrap();
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left());
    assert!(no_netstack_left(), "the netstack outlived its VM");
}

/// (c) With the netstack installed, network `disabled` still has no NIC and spawns no
/// netstack; killing the helper takes the netstack down with it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun + pinned netstack: signed helper, pinned sandcrab/base:0.1.0, static crabd"]
async fn userspace_disabled_has_no_nic_and_netstack_dies_with_helper() {
    let env = env_net();
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap();
    assert_loopback_only(&sb).await;
    assert!(no_netstack_left(), "`disabled` spawned a netstack");
    sb.destroy().await.unwrap();

    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base").network(NetworkConfig::unrestricted()))
        .await
        .unwrap();
    assert!(!no_netstack_left_now(), "no netstack for `unrestricted`");
    // SIGKILL this sandbox's helper only (its pid from the run dir's meta.json; never a
    // pattern kill on a shared host): no cleanup code runs, the netstack must exit on its own.
    let meta = sandcrab_backend_libkrun::reap::RunMeta::load(
        &env.tmp.path().join("run").join(sb.id().to_string()),
    )
    .unwrap();
    let helper = meta.helper.expect("helper pid in meta.json").pid;
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(helper as i32).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    assert!(
        no_netstack_left(),
        "the netstack outlived a SIGKILLed helper"
    );
    let _ = sb.destroy().await;
    assert!(no_helper_left());
}

fn no_netstack_left_now() -> bool {
    netstacks_now().is_empty()
}

/// Linux: with `$SANDCRAB_LIBKRUN_CGROUP` (a delegated cgroup v2 parent; the test process must
/// run inside the delegated tree, e.g. under `systemd-run --user --scope`) the helper runs in
/// its own leaf with `memory.max` = guest RAM + VMM overhead and `pids.max`, and the leaf is
/// removed on destroy. Skipped (with a note) when the variable is unset.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun on Linux + a delegated cgroup in $SANDCRAB_LIBKRUN_CGROUP"]
async fn helper_runs_in_its_cgroup_leaf() {
    let Some(parent) = sandcrab_backend_libkrun::config::cgroup_parent_from_env() else {
        println!("SKIP: SANDCRAB_LIBKRUN_CGROUP is not set");
        return;
    };
    let env = env_cfg(false, |c| c.cgroup_parent = Some(parent.clone()));
    let overhead = env.backend.config().vmm_overhead_mib;
    let pids_max = env.backend.config().pids_max;
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap();
    let leaf = parent.join(format!("sandcrab-{}", sb.id()));
    let read = |f: &str| std::fs::read_to_string(leaf.join(f)).unwrap();
    let mem_mb = bash(&sb, "awk '/MemTotal/{print $2}' /proc/meminfo").await;
    assert_eq!(mem_mb.exit_code, Some(0), "{mem_mb:?}");
    let procs: Vec<u32> = read("cgroup.procs")
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    assert_eq!(procs.len(), 1, "only the helper: {procs:?}");
    // libkrun renames the VMM thread (`comm` = "libkrun VM"); the executable is the helper.
    let exe = std::fs::read_link(format!("/proc/{}/exe", procs[0])).unwrap();
    assert!(exe.ends_with("sandcrab-krun"), "{}", exe.display());
    let memory_max: u64 = read("memory.max").trim().parse().unwrap();
    assert!(
        memory_max > overhead << 20,
        "memory.max {memory_max} covers guest RAM + {overhead} MiB"
    );
    assert_eq!(read("pids.max").trim(), pids_max.to_string());
    let current: u64 = read("memory.current").trim().parse().unwrap();
    println!(
        "CGROUP leaf memory.max={} MiB memory.current={} MiB pids.current={}",
        memory_max >> 20,
        current >> 20,
        read("pids.current").trim()
    );
    assert!(current > 0 && current <= memory_max);
    sb.destroy().await.unwrap();
    assert!(!leaf.exists(), "leaf removed on destroy");

    // Dropped with no runtime left to run a destroy (a panicking caller whose runtime shuts
    // down): the VM's own Drop kills the helper and must still remove the leaf.
    let backend = env.backend.clone();
    let state = env.tmp.path().join("state2");
    let id = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (manager, sb) = rt.block_on(async {
            let manager = SandboxManager::new(
                backend,
                WorkspaceRegistry::open(state).unwrap(),
                TemplateRegistry::load(&[root().join("templates")]).unwrap(),
                ManagerConfig::default(),
            )
            .unwrap();
            let sb = manager
                .spawn(SandboxConfig::from_template("base"))
                .await
                .unwrap();
            (manager, sb)
        });
        let id = sb.id();
        // Outside `block_on` there is no current runtime: the manager cannot schedule a
        // destroy and drops the handle while the helper is still running.
        drop(sb);
        drop(manager);
        drop(rt);
        id
    })
    .join()
    .unwrap();
    let leaf = parent.join(format!("sandcrab-{id}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while leaf.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!leaf.exists(), "leaf removed after drop");
    assert!(no_helper_left());
    assert!(leftovers(&env).is_empty());
}

/// Start `cmd` as a managed background process and wait for `LISTENING` on its output.
async fn start_and_wait(sb: &Sandbox, cmd: &str) -> ProcessStartResult {
    let proc = sb
        .call::<ProcessStartResult>(ProcessStartParams {
            command: cmd.into(),
            cwd: None,
            env: Default::default(),
            pty: false,
        })
        .await
        .unwrap();
    let mut out = String::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !out.contains("LISTENING") {
        assert!(Instant::now() < deadline, "never listened: {out}");
        let p = sb
            .call::<ProcessPollResult>(ProcessPollParams {
                process_id: proc.process_id.clone(),
            })
            .await
            .unwrap();
        out.push_str(&p.stdout);
        out.push_str(&p.stderr);
        assert!(p.running, "exited: {p:?} ({out})");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    proc
}

/// Guest servers for the port test (node, from the `ts-dev` template):
/// - 8080: HTTP (keep-alive, `Connection: close`, HTTP/1.0);
/// - 7001: reads a `<len>\n` header, counts `len` body bytes, replies `got <n>` (upload);
/// - 7002: sends `hello`, half-closes, records what the client sends afterwards;
/// - 7004: counts bytes until the client's EOF, then replies `eof <n>` (host → guest
///   half-close).
const PORT_SERVERS_JS: &str = r"
const net = require('net'), http = require('http'), fs = require('fs');
http.createServer((q, s) => s.end('hello from krun ' + q.url)).listen(8080, '127.0.0.1');
net.createServer({ allowHalfOpen: true }, s => {
  let head = '', want = -1, n = 0;
  s.on('data', d => {
    if (want < 0) {
      head += d.toString('latin1');
      const i = head.indexOf('\n');
      if (i < 0) return;
      want = parseInt(head.slice(0, i), 10);
      n = Buffer.byteLength(head.slice(i + 1), 'latin1');
    } else n += d.length;
    if (n >= want) s.end('got ' + n);
  });
  s.on('error', () => {});
}).listen(7001, '127.0.0.1');
net.createServer({ allowHalfOpen: true }, s => {
  let got = '';
  s.on('data', d => { got += d; fs.writeFileSync('/tmp/7002.txt', got); });
  s.on('error', () => {});
  s.end('hello');
}).listen(7002, '127.0.0.1');
net.createServer({ allowHalfOpen: true }, s => {
  let n = 0;
  s.on('data', d => { n += d.length; });
  s.on('end', () => s.end('eof ' + n));
  s.on('error', () => {});
}).listen(7004, '127.0.0.1', () => console.log('LISTENING'));
";

/// Read until `needle` shows up, EOF, or `wait` passes. Returns the bytes and whether EOF was
/// seen.
async fn read_until(
    r: &mut tokio::net::tcp::OwnedReadHalf,
    needle: &str,
    wait: Duration,
) -> (String, bool) {
    use tokio::io::AsyncReadExt;
    let mut got = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return (String::from_utf8_lossy(&got).into_owned(), false);
        }
        match tokio::time::timeout(left, r.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => return (String::from_utf8_lossy(&got).into_owned(), true),
            Ok(Ok(n)) => {
                got.extend_from_slice(&buf[..n]);
                if !needle.is_empty() && String::from_utf8_lossy(&got).contains(needle) {
                    return (String::from_utf8_lossy(&got).into_owned(), false);
                }
            }
            Err(_) => return (String::from_utf8_lossy(&got).into_owned(), false),
        }
    }
}

/// Published ports over `v.sock` + `tunnel.open` on real libkrun, network `disabled`.
///
/// Must work: HTTP keep-alive, `Connection: close` (the server's FIN reaches the client),
/// HTTP/1.0 with the client's `SHUT_WR`, a 4 MiB upload, a refused port, listeners closing on
/// destroy.
///
/// Measured libkrun behaviour (asserted so a change is noticed): a host `SHUT_WR` on `v.sock`
/// breaks the stream, so the backend does not forward the client's half-close
/// (`ports::HOST_HALF_CLOSE`) and upload-until-EOF gets no reply. A guest `SHUT_WR` does reach
/// the host, but `crabd` closes vsock tunnels on target EOF (same as Firecracker), so the host
/// cannot send after the guest's FIN.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/ts-dev:0.1.0, static crabd"]
async fn published_ports_over_vsock_tunnels() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    let env = env();
    let sb = env
        .manager
        .spawn(
            SandboxConfig::from_template("ts-dev")
                .port(8080)
                .port(7001)
                .port(7002)
                .port(7003)
                .port(7004),
        )
        .await
        .unwrap();
    let ports = sb.published_ports().to_vec();
    let host = |guest: u16| {
        ports
            .iter()
            .find(|p| p.guest == guest)
            .expect("published")
            .host
    };
    for p in &ports {
        assert_eq!(p.host.ip(), Ipv4Addr::LOCALHOST, "{p:?}");
    }
    assert_loopback_only(&sb).await;

    // Nothing listens yet: refused per connection (closed without data).
    let (mut r, _w) = TcpStream::connect(host(7003)).await.unwrap().into_split();
    assert_eq!(
        read_until(&mut r, "", Duration::from_secs(10)).await,
        (String::new(), true)
    );

    bash(
        &sb,
        &format!("cat > /tmp/s.js <<'EOF'\n{PORT_SERVERS_JS}\nEOF"),
    )
    .await;
    let _proc = start_and_wait(&sb, "node /tmp/s.js").await;
    let wait = Duration::from_secs(10);

    // HTTP/1.1 keep-alive.
    let (mut r, mut w) = TcpStream::connect(host(8080)).await.unwrap().into_split();
    w.write_all(b"GET /ka HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let (got, _) = read_until(&mut r, "hello from krun /ka", wait).await;
    assert!(got.contains("hello from krun /ka"), "{got:?}");
    w.write_all(b"GET /ka2 HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let (got, _) = read_until(&mut r, "hello from krun /ka2", wait).await;
    assert!(got.contains("hello from krun /ka2"), "{got:?}");
    drop((r, w));

    // `Connection: close`: response and then the server's FIN (read-until-close clients).
    let (mut r, mut w) = TcpStream::connect(host(8080)).await.unwrap().into_split();
    w.write_all(b"GET /close HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let (got, eof) = read_until(&mut r, "", Duration::from_secs(5)).await;
    println!("PROBE guest FIN reaches host (Connection: close): {eof}");
    assert!(got.contains("hello from krun /close"), "{got:?}");
    assert!(eof, "server FIN not forwarded: {got:?}");
    drop((r, w));

    // HTTP/1.0 with the client half-closing after its request: the reply still arrives.
    let (mut r, mut w) = TcpStream::connect(host(8080)).await.unwrap().into_split();
    w.write_all(b"GET /half HTTP/1.0\r\n\r\n").await.unwrap();
    w.shutdown().await.unwrap();
    let (got, eof) = read_until(&mut r, "", Duration::from_secs(5)).await;
    assert!(got.contains("hello from krun /half"), "{got:?}");
    assert!(eof, "{got:?}");
    drop(r);

    // 4 MiB upload, reply when complete.
    let (mut r, mut w) = TcpStream::connect(host(7001)).await.unwrap().into_split();
    let len = 4 * 1024 * 1024;
    w.write_all(format!("{len}\n").as_bytes()).await.unwrap();
    w.write_all(&vec![7u8; len]).await.unwrap();
    let want = format!("got {len}");
    let (got, _) = read_until(&mut r, &want, Duration::from_secs(30)).await;
    assert_eq!(got, want);
    drop((r, w));

    // Limitation: the client's SHUT_WR is not forwarded, so upload-until-EOF gets no reply.
    let (mut r, mut w) = TcpStream::connect(host(7004)).await.unwrap().into_split();
    w.write_all(&vec![1u8; 100_000]).await.unwrap();
    w.shutdown().await.unwrap();
    let (got, eof) = read_until(&mut r, "", Duration::from_secs(2)).await;
    println!("PROBE client half-close reaches guest: got={got:?} eof={eof}");
    assert_eq!(
        (got.as_str(), eof),
        ("", false),
        "the client's EOF now reaches the guest: revisit ports::HOST_HALF_CLOSE and docs"
    );
    drop(r);

    // Guest half-closes after `hello`: the host gets the bytes and then EOF (crabd closes the
    // tunnel); later host bytes are dropped.
    let (mut r, mut w) = TcpStream::connect(host(7002)).await.unwrap().into_split();
    let (got, eof) = read_until(&mut r, "", Duration::from_secs(5)).await;
    assert_eq!((got.as_str(), eof), ("hello", true));
    let _ = w.write_all(b"after-guest-fin").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let seen = bash(&sb, "cat /tmp/7002.txt 2>/dev/null").await.stdout;
    println!("PROBE host bytes after the guest's FIN: guest got {seen:?}");
    assert!(!seen.contains("after-guest-fin"), "{seen:?}");
    drop((r, w));

    let marks = sb.destroy().await.unwrap();
    assert!(marks.custom.iter().any(|(n, _)| n == "ports_published"));
    for p in &ports {
        assert!(
            TcpStream::connect(p.host).await.is_err(),
            "{p:?} open after destroy"
        );
    }
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left());
}

// ---------------------------------------------------------------------------------------------
// Verified fds (design §12.4): tampering after install is refused at spawn; review R8.

/// `cp -a` (macOS: `cp -c` clones on APFS, so a rootfs copy costs no space).
fn copy_tree(src: &Path, dst: &Path) {
    let mut cmd = std::process::Command::new("cp");
    if cfg!(target_os = "macos") {
        cmd.arg("-c");
    }
    let st = cmd.arg("-a").arg(src).arg(dst).status().unwrap();
    assert!(st.success(), "cp {} {}", src.display(), dst.display());
}

/// An env whose helper (+ its record), libraries, netstack and `base` rootfs cache entries are
/// private copies under the test's tmp dir, so the test may tamper with them.
fn private_env(netstack: bool) -> Env {
    private_env_libs(netstack, |_| {})
}

fn private_parts(
    netstack: bool,
    tweak_libs: impl FnOnce(&mut KrunLibs),
) -> (tempfile::TempDir, LibkrunConfig, KrunLibs) {
    let tmp = tempfile::Builder::new()
        .prefix("sclk")
        .tempdir_in("/tmp")
        .unwrap();
    let t = tmp.path().to_path_buf();
    let (cfg, _) = config(&t, false, netstack);
    copy_tree(&Artifacts::resolve(&cfg).unwrap().dir, &t.join("lib"));
    if let Some(ns) = &cfg.netstack_dir {
        copy_tree(ns, &t.join("net"));
    }
    // 0700 whatever the umask: the backend refuses directories others can write (review R18).
    let private_dir = |p: &Path| {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(p)
            .unwrap();
    };
    private_dir(&t.join("bin"));
    let helper = t.join("bin").join("sandcrab-krun");
    copy_tree(&cfg.helper, &helper);
    sandcrab_backend_libkrun::verified::HelperRecord::record(&helper).unwrap();
    let dst_rootfs = t.join("cache").join("rootfs");
    private_dir(&dst_rootfs);
    for e in std::fs::read_dir(cfg.cache_root.join("rootfs"))
        .unwrap()
        .flatten()
    {
        let m = std::fs::read_to_string(e.path().join("manifest.json")).unwrap_or_default();
        if m.contains("\"template\": \"base\"") {
            copy_tree(&e.path(), &dst_rootfs.join(e.file_name()));
        }
    }
    let (cfg, mut libs) = config_with(&t, false, netstack, |c| {
        c.lib_dir = Some(t.join("lib"));
        c.netstack_dir = Some(t.join("net"));
        c.helper = helper.clone();
        c.cache_root = t.join("cache");
    });
    tweak_libs(&mut libs);
    (tmp, cfg, libs)
}

fn private_env_libs(netstack: bool, tweak_libs: impl FnOnce(&mut KrunLibs)) -> Env {
    let (tmp, cfg, libs) = private_parts(netstack, tweak_libs);
    env_from(tmp, cfg, libs)
}

/// Review N2: libkrunfw replaced by another file whose genuine stamp (bound to its own digest)
/// is pasted into the pin: no stamp hit, refused at construction.
#[test]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
fn stamp_of_another_file_is_refused() {
    let (_tmp, cfg, libs) = private_parts(false, |l| {
        let p = l.libkrunfw.path.clone();
        let evil = p.with_extension("evil");
        std::fs::write(&evil, b"not libkrunfw").unwrap();
        std::fs::rename(&evil, &p).unwrap();
        l.libkrunfw.stamp = Some(sandcrab_rootfs::stamp::record(&p).unwrap().1);
    });
    let e = LibkrunBackend::new(cfg, libs).unwrap_err();
    assert!(matches!(e, SandboxError::DigestMismatch { .. }), "{e}");
}

/// Review R18 at backend construction: inside the state root a group-writable directory is
/// tightened to 0700; outside it (an override) it is refused and left alone.
#[test]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
fn loose_state_dirs_are_tightened_or_refused() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let set = |p: &Path, m: u32| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    };
    // Override outside the state root: refused, never chmodded.
    let (tmp, cfg, libs) = private_parts(false, |_| {});
    let bin = tmp.path().join("bin");
    set(&bin, 0o775);
    let e = LibkrunBackend::new(cfg.clone(), libs.clone()).unwrap_err();
    assert!(e.to_string().contains("chmod go-w"), "{e}");
    assert_eq!(mode(&bin), 0o775);
    // The same tree as the state root: tightened, then the backend comes up.
    let mut cfg = cfg;
    cfg.state_root = Some(tmp.path().to_path_buf());
    set(tmp.path(), 0o775);
    set(&tmp.path().join("lib"), 0o777);
    LibkrunBackend::new(cfg.clone(), libs.clone()).unwrap();
    for d in [tmp.path(), &bin, &tmp.path().join("lib")] {
        assert_eq!(mode(d), 0o700, "{}", d.display());
    }
    // A symlinked state root is refused.
    let link = tmp.path().with_extension("link");
    std::os::unix::fs::symlink(tmp.path(), &link).unwrap();
    cfg.state_root = Some(link.clone());
    let e = LibkrunBackend::new(cfg, libs).unwrap_err();
    std::fs::remove_file(&link).unwrap();
    assert!(e.to_string().contains("symlink"), "{e}");
}

/// The `base` rootfs image(s) under the env's private cache.
fn private_rootfs(env: &Env) -> Vec<PathBuf> {
    let imgs: Vec<_> = std::fs::read_dir(env.tmp.path().join("cache").join("rootfs"))
        .unwrap()
        .flatten()
        .map(|e| e.path().join("rootfs.ext4"))
        .filter(|p| p.is_file())
        .collect();
    assert!(
        !imgs.is_empty(),
        "no base rootfs in the cache: spawn base once first"
    );
    imgs
}

fn with_owner_write(path: &Path, f: impl FnOnce()) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).unwrap().permissions().mode();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o200)).unwrap();
    f();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Overwrite one byte at `offset` in place (same inode, same size), then put the mtime back:
/// only ctime tells.
fn poke(path: &Path, offset: u64) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mtime = std::fs::metadata(path).unwrap().modified().unwrap();
    with_owner_write(path, || {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let mut b = [0u8; 1];
        f.seek(SeekFrom::Start(offset)).unwrap();
        f.read_exact(&mut b).unwrap();
        f.seek(SeekFrom::Start(offset)).unwrap();
        f.write_all(&[!b[0]]).unwrap();
        f.set_modified(mtime).unwrap();
        f.sync_all().unwrap();
    });
}

/// Replace `path` by a modified copy under the same name (new inode, same size).
fn swap_modified(path: &Path, offset: u64) {
    let tmp = path.with_extension("evil");
    copy_tree(path, &tmp);
    poke(&tmp, offset);
    std::fs::rename(&tmp, path).unwrap();
}

async fn spawn_base(env: &Env) -> Result<Sandbox, SandboxError> {
    env.manager
        .spawn(SandboxConfig::from_template("base"))
        .await
}

fn helper_refused_pin(e: &SandboxError) -> bool {
    e.to_string().contains("exit code 71")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
async fn tampered_rootfs_is_refused() {
    let env = private_env(false);
    // First spawn: the copy's stamp misses, so it is re-hashed and re-stamped; then it boots.
    let sb = spawn_base(&env).await.unwrap();
    assert_eq!(bash(&sb, "true").await.exit_code, Some(0));
    sb.destroy().await.unwrap();
    let img = private_rootfs(&env).remove(0);
    // A benign metadata change (chmod u+w and back): re-hashed, accepted.
    with_owner_write(&img, || {});
    spawn_base(&env).await.unwrap().destroy().await.unwrap();
    // Modified in place (same inode, size and mtime): refused.
    poke(&img, 4096 * 3);
    let e = spawn_base(&env).await.unwrap_err();
    assert!(matches!(e, SandboxError::DigestMismatch { .. }), "{e}");
    // Put back, then replaced under the same name: refused.
    poke(&img, 4096 * 3);
    spawn_base(&env).await.unwrap().destroy().await.unwrap();
    swap_modified(&img, 4096 * 5);
    let e = spawn_base(&env).await.unwrap_err();
    assert!(matches!(e, SandboxError::DigestMismatch { .. }), "{e}");
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
async fn tampered_libraries_are_refused() {
    // libkrun modified in place after the backend checked it: the helper refuses (71).
    let env = private_env(false);
    let libs = env.backend.libs().clone();
    spawn_base(&env).await.unwrap().destroy().await.unwrap();
    poke(&libs.libkrun.path, 4096);
    let e = spawn_base(&env).await.unwrap_err();
    assert!(helper_refused_pin(&e), "{e}");
    assert!(leftovers(&env).is_empty());
    // libkrunfw replaced under the same name.
    let env = private_env(false);
    let libs = env.backend.libs().clone();
    swap_modified(&libs.libkrunfw.path, 4096);
    let e = spawn_base(&env).await.unwrap_err();
    assert!(helper_refused_pin(&e), "{e}");
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
async fn replaced_helper_is_refused() {
    let env = private_env(false);
    let helper = env.backend.config().helper.clone();
    let len = std::fs::metadata(&helper).unwrap().len();
    spawn_base(&env).await.unwrap().destroy().await.unwrap();
    // Modified in place: the backend refuses before launching it.
    poke(&helper, len / 2);
    let e = spawn_base(&env).await.unwrap_err();
    assert!(matches!(e, SandboxError::DigestMismatch { .. }), "{e}");
    // Replaced under the same name.
    let env = private_env(false);
    let helper = env.backend.config().helper.clone();
    swap_modified(&helper, len / 3);
    let e = spawn_base(&env).await.unwrap_err();
    assert!(matches!(e, SandboxError::DigestMismatch { .. }), "{e}");
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun + pinned netstack: signed helper, pinned sandcrab/base:0.1.0, static crabd"]
async fn tampered_netstack_is_refused() {
    let unrestricted =
        || SandboxConfig::from_template("base").network(NetworkConfig::unrestricted());
    #[cfg(target_arch = "x86_64")]
    let avx2 = std::arch::is_x86_feature_detected!("avx2");
    #[cfg(not(target_arch = "x86_64"))]
    let avx2 = false;
    let env = private_env(true);
    let ns = env.backend.libs().netstack.clone().unwrap();
    let sb = env.manager.spawn(unrestricted()).await.unwrap();
    sb.destroy().await.unwrap();
    assert!(
        no_netstack_left(),
        "the netstack exits (else: ETXTBSY below)"
    );
    // The binary the helper executes on this CPU, modified in place.
    let executed = match (&ns.avx2, avx2) {
        (Some(v), true) => v.path.clone(),
        _ => ns.binary.path.clone(),
    };
    poke(&executed, 8192);
    let e = env.manager.spawn(unrestricted()).await.unwrap_err();
    assert!(helper_refused_pin(&e), "{e}");
    assert!(leftovers(&env).is_empty());
    // passt on an AVX2 CPU without a pinned variant: the `.avx2` sibling passt would exec by
    // path is unpinned, so the helper refuses.
    if ns.avx2.is_some() && avx2 {
        let env = private_env_libs(true, |l| l.netstack.as_mut().unwrap().avx2 = None);
        let e = env.manager.spawn(unrestricted()).await.unwrap_err();
        assert!(helper_refused_pin(&e), "{e}");
        assert!(leftovers(&env).is_empty());
    }
    assert!(no_helper_left());
}

#[test]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
fn vm_survives_the_thread_that_spawned_it() {
    // Review R8: PR_SET_PDEATHSIG fires when the *thread* that forked the helper exits. Spawn
    // from a short-lived thread, let it exit, and check that the VM still serves.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_keep_alive(Duration::from_millis(50))
        .build()
        .unwrap();
    let _guard = rt.enter();
    let env = env();
    let sb = std::thread::scope(|s| {
        s.spawn(|| rt.block_on(spawn_base(&env)).unwrap())
            .join()
            .unwrap()
    });
    std::thread::sleep(Duration::from_millis(1500));
    rt.block_on(async {
        let r = bash(&sb, "echo alive").await;
        assert_eq!(r.stdout.trim(), "alive", "{r:?}");
        sb.destroy().await.unwrap();
    });
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left());
}

/// Third-party images for external templates, pinned by multi-arch index digest: Node, git and
/// Python (`node:24-bookworm`), and one with no shell whose `USER` is not root
/// (`distroless/static-debian12:nonroot`).
const EXTERNAL_NODE_IMAGE: &str =
    "node@sha256:49f7adaf9f1574bc45c20ea317139b39a59d7e758d0e93745e112fd85cb42623";
const EXTERNAL_NOSHELL_IMAGE: &str = "gcr.io/distroless/static-debian12@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab";

/// External templates (bring your own image) boot through the rootfs pipeline: refused until
/// this host pins them, then a Node + git + Python image runs its programs, and an image with
/// no shell still serves the file tools.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, static crabd, network to pull public images"]
async fn external_templates_boot_after_pin() {
    let tmp = tempfile::Builder::new()
        .prefix("sclk")
        .tempdir_in("/tmp")
        .unwrap();
    let (cfg, libs) = config(tmp.path(), false, false);
    let backend = Arc::new(LibkrunBackend::new(cfg, libs).unwrap());
    let tdir = tmp.path().join("templates");
    for (name, image, tiers) in [
        ("node-ext", EXTERNAL_NODE_IMAGE, r#"["core", "process"]"#),
        ("noshell-ext", EXTERNAL_NOSHELL_IMAGE, r#"["core"]"#),
    ] {
        let pull = std::process::Command::new("docker")
            .args(["pull", "-q", image])
            .status()
            .unwrap();
        assert!(pull.success(), "docker pull {image}");
        std::fs::create_dir_all(tdir.join(name)).unwrap();
        std::fs::write(
            tdir.join(name).join("sandcrab.toml"),
            format!(
                "[template]\nname = \"{name}\"\nversion = \"1.0.0\"\nimage = \"{image}\"\n\
                 external = true\n\n[resources]\nvcpu = 1\nmemory_mb = 512\ndisk_mb = 2048\n\n\
                 [tools]\ntiers = {tiers}\n"
            ),
        )
        .unwrap();
    }
    let templates = TemplateRegistry::load(&[&tdir]).unwrap();
    let pins = sandcrab_core::PinStore::open(tmp.path().join("pins"));
    let manager = SandboxManager::new(
        backend,
        WorkspaceRegistry::open(tmp.path().join("state")).unwrap(),
        templates.clone(),
        ManagerConfig {
            template_pins: Some(pins.clone()),
            ..ManagerConfig::default()
        },
    )
    .unwrap();

    // Unpinned: refused before anything boots, naming the fix.
    let err = manager
        .spawn(SandboxConfig::from_template("node-ext"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("UnpinnedImage"), "{err}");
    assert!(err.contains("sandcrab template pin node-ext"), "{err}");
    assert!(no_helper_left());

    // What `sandcrab template pin` does.
    let arch = sandcrab_core::ImageArch::host().unwrap();
    for name in ["node-ext", "noshell-ext"] {
        let t = templates.get(name).unwrap();
        let id = sandcrab_rootfs::docker_image_id(t.image()).unwrap();
        pins.adopt(&t.manifest, arch, &id).unwrap();
    }

    let sb = manager
        .spawn(SandboxConfig::from_template("node-ext").require_hardware_isolation(true))
        .await
        .unwrap();
    let r = bash(
        &sb,
        "node --version && git --version && python3 --version && cat /proc/1/comm",
    )
    .await;
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    let lines: Vec<_> = r.stdout.lines().collect();
    assert!(lines[0].starts_with('v'), "{}", r.stdout);
    assert!(lines[1].starts_with("git version"), "{}", r.stdout);
    assert!(lines[2].starts_with("Python 3"), "{}", r.stdout);
    assert_eq!(lines[3], "crabd", "crabd is PID 1");
    sb.destroy().await.unwrap();

    let sb = manager
        .spawn(SandboxConfig::from_template("noshell-ext"))
        .await
        .unwrap();
    let w = sb
        .call::<FileWriteResult>(FileWriteParams {
            path: "/tmp/hello.txt".into(),
            content: "no shell needed\n".into(),
            append: false,
        })
        .await;
    assert!(w.is_ok(), "{w:?}");
    let l = sb
        .call::<ListDirResult>(ListDirParams {
            path: Some("/tmp".into()),
            depth: Some(1),
            respect_gitignore: false,
        })
        .await
        .unwrap();
    assert!(
        l.entries.iter().any(|e| e.path.ends_with("hello.txt")),
        "{l:?}"
    );
    sb.destroy().await.unwrap();
    assert!(no_helper_left());
}

// ---------------------------------------------------------------------------------------
// Server options (sdk-milestone): `/tmp`, process limits, process streams, host mounts at
// the same path and forwards, all inside a real guest.
// ---------------------------------------------------------------------------------------

fn uuid() -> String {
    sandcrab_core::SandboxId::new().to_string()
}

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}

fn open_params(a: &[&str]) -> ProcessOpenParams {
    ProcessOpenParams::new(uuid(), argv(a))
}

/// Everything a finished `process.open` program sent.
struct Ran {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: ProcessExit,
}

/// Read a process to its exit. The exit must be the last event.
async fn drain(out: &mut ProcessOutput) -> Ran {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    loop {
        match out.next().await.unwrap().expect("EXIT before the end") {
            ProcessEvent::Stdout(b) => stdout.extend_from_slice(&b),
            ProcessEvent::Stderr(b) => stderr.extend_from_slice(&b),
            ProcessEvent::Exit(exit) => {
                assert!(out.next().await.unwrap().is_none(), "EXIT is last");
                return Ran {
                    stdout,
                    stderr,
                    exit,
                };
            }
        }
    }
}

async fn run(sb: &Sandbox, p: ProcessOpenParams) -> Ran {
    let mut s = sb.open_process(p).await.unwrap();
    drain(s.output()).await
}

fn p50(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
async fn tmp_and_process_limit_options_take_effect() {
    let env = env();
    let caps = env.backend.capabilities();
    assert!(caps.supports_tmp_options && caps.supports_process_limits);
    assert_eq!(caps.nproc_scope, NprocScope::Sandbox);
    const PROBE: &str = "findmnt -no OPTIONS /tmp; stat -c 'mode=%a' /tmp; \
         cp /bin/true /tmp/t && /tmp/t; echo exec_rc=$?; \
         echo pids_max=$(cat /sys/fs/cgroup/sandbox/pids.max); cat /proc/self/cgroup; \
         echo nofile=$(ulimit -Sn):$(ulimit -Hn); \
         touch /sys/fs/cgroup/sandbox/pids.max 2>&1; findmnt -no FSTYPE /";

    // Defaults: noexec 256 MiB /tmp, 1024 pids.
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap();
    let r = bash(&sb, PROBE).await;
    println!("OPTIONS default:\n{}", r.stdout);
    let opts = r.stdout.lines().next().unwrap().to_owned();
    assert!(opts.contains("noexec") && opts.contains("nosuid") && opts.contains("nodev"));
    assert!(opts.contains("size=262144k"), "{opts}");
    assert!(r.stdout.contains("mode=1777"), "{}", r.stdout);
    assert!(r.stdout.contains("exec_rc=126"), "{}", r.stdout);
    assert!(r.stdout.contains("pids_max=1024"), "{}", r.stdout);
    sb.destroy().await.unwrap();

    // Every option at once.
    let sb = env
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .tmp_exec(true)
                .tmp_size_mb(64)
                .resources(ResourceOverrides {
                    pids_max: Some(200),
                    nofile: Some(4096),
                    ..ResourceOverrides::default()
                }),
        )
        .await
        .unwrap();
    let r = bash(&sb, PROBE).await;
    println!("OPTIONS set:\n{}", r.stdout);
    let opts = r.stdout.lines().next().unwrap().to_owned();
    assert!(!opts.contains("noexec"), "{opts}");
    assert!(opts.contains("nosuid") && opts.contains("nodev"), "{opts}");
    assert!(opts.contains("size=65536k"), "{opts}");
    assert!(r.stdout.contains("mode=1777"), "{}", r.stdout);
    assert!(r.stdout.contains("exec_rc=0"), "{}", r.stdout);
    assert!(r.stdout.contains("pids_max=200"), "{}", r.stdout);
    assert!(r.stdout.contains("0::/sandbox"), "{}", r.stdout);
    assert!(r.stdout.contains("nofile=4096:4096"), "{}", r.stdout);
    // The cap cannot be raised from inside (and hardening held: it fails closed otherwise).
    assert!(r.stdout.contains("cannot touch"), "{}", r.stdout);

    // /tmp really is 64 MiB: 60 fit (next to the copied binary), 8 more do not.
    let r = bash(
        &sb,
        "head -c 62914560 /dev/zero > /tmp/a; echo rc=$?; \
         head -c 8388608 /dev/zero 2>&1 > /tmp/b; echo rc=$?",
    )
    .await;
    assert!(
        r.stdout.starts_with("rc=0") && r.stdout.contains("No space left"),
        "{r:?}"
    );

    // pids.max bounds the whole sandbox: a 300-task burst runs into it (the shell dies of
    // the failed fork) and the kernel counts the refusals.
    let r = run(
        &sb,
        open_params(&[
            "/bin/sh",
            "-c",
            "i=0; while [ $i -lt 300 ]; do sleep 20 & i=$((i+1)); done; echo not-reached",
        ]),
    )
    .await;
    assert!(
        r.stdout.is_empty() && r.exit.code != Some(0),
        "{:?}",
        r.exit
    );
    let r = bash(&sb, "cat /sys/fs/cgroup/sandbox/pids.events").await;
    println!(
        "OPTIONS pids.events after a 300-task burst with pids_max=200: {}",
        r.stdout.trim()
    );
    let hits: u64 = r
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("max "))
        .unwrap_or_else(|| panic!("{r:?}"))
        .trim()
        .parse()
        .unwrap();
    assert!(hits >= 1, "{r:?}");

    // A process may lower its own nofile, never pass the sandbox's.
    let mut p = open_params(&["/bin/sh", "-c", "ulimit -n"]);
    p.limits = Some(ExecLimits {
        nofile: Some(128),
        ..ExecLimits::default()
    });
    assert_eq!(run(&sb, p).await.stdout, b"128\n");
    let mut p = open_params(&["/bin/true"]);
    p.limits = Some(ExecLimits {
        nofile: Some(8192),
        ..ExecLimits::default()
    });
    let err = sb.open_process(p).await.unwrap_err();
    assert!(
        matches!(&err, SandboxError::Protocol(e) if e.code() == sandcrab_protocol::codes::INVALID_PARAMS),
        "{err}"
    );

    sb.destroy().await.unwrap();
    assert!(leftovers(&env).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
async fn process_streams_over_vsock() {
    let env = env();
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base").env("FROM_SANDBOX", "never-inherited"))
        .await
        .unwrap();

    // Exact env and argv, no shell in between.
    let mut p = open_params(&["/usr/bin/env"]);
    p.env = [("A", "1"), ("B", "x y"), ("EMPTY", "")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let r = run(&sb, p).await;
    assert_eq!(
        String::from_utf8_lossy(&r.stdout),
        "A=1\nB=x y\nEMPTY=\n",
        "{:?}",
        r.exit
    );
    assert_eq!((r.exit.code, r.exit.signal), (Some(0), None));
    assert!(r.exit.tree_clean && !r.exit.timed_out && !r.exit.cancelled);
    let r = run(
        &sb,
        open_params(&[
            "/bin/sh",
            "-c",
            "printf '%s|' \"$0\" \"$@\"",
            "zero",
            "a b",
            "",
            "$HOME",
            "*",
            "é\n",
        ]),
    )
    .await;
    assert_eq!(String::from_utf8_lossy(&r.stdout), "zero|a b||$HOME|*|é\n|");

    // 1 MiB of every byte value through stdin and back: identical, in order, exit last.
    let data: Vec<u8> = (0..1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let mut p = open_params(&["/bin/cat"]);
    p.stdin = StdinMode::Pipe;
    let t0 = Instant::now();
    let (mut stdin, mut out) = sb.open_process(p).await.unwrap().into_split();
    let sent = data.clone();
    let writer = tokio::spawn(async move {
        stdin.write(&sent).await.unwrap();
        stdin.close().await.unwrap();
        stdin
    });
    let r = drain(&mut out).await;
    writer.await.unwrap();
    assert_eq!(r.stdout.len(), data.len());
    assert!(r.stdout == data, "1 MiB arrived changed");
    assert_eq!(r.exit.code, Some(0));
    println!(
        "PROCESS 1 MiB stdin->cat->stdout round trip: {:?}",
        t0.elapsed()
    );

    // stdin: null is an immediate EOF; a pipe stays open until STDIN_EOF.
    let r = run(&sb, open_params(&["/bin/cat"])).await;
    assert!(
        r.stdout.is_empty() && r.exit.code == Some(0),
        "{:?}",
        r.exit
    );
    let mut p = open_params(&["/bin/cat"]);
    p.stdin = StdinMode::Pipe;
    let mut s = sb.open_process(p).await.unwrap();
    s.stdin().write(b"ping\n").await.unwrap();
    assert_eq!(
        s.output().next().await.unwrap(),
        Some(ProcessEvent::Stdout(b"ping\n".to_vec()))
    );
    let quiet = tokio::time::timeout(Duration::from_millis(400), s.output().next()).await;
    assert!(quiet.is_err(), "cat ended while its stdin was open");
    s.stdin().close().await.unwrap();
    let r = drain(s.output()).await;
    assert!(
        r.stdout.is_empty() && r.exit.code == Some(0),
        "{:?}",
        r.exit
    );

    // Cancel stops a grandchild in its own session that ignores SIGTERM.
    let p = open_params(&[
        "/bin/sh",
        "-c",
        "setsid sh -c 'trap \"\" TERM; echo $$; exec sleep 1000' & wait",
    ]);
    let id = p.process_id.clone();
    let mut s = sb.open_process(p).await.unwrap();
    let Some(ProcessEvent::Stdout(line)) = s.output().next().await.unwrap() else {
        panic!("no pid line");
    };
    let grandchild: u32 = String::from_utf8_lossy(&line).trim().parse().unwrap();
    let r = bash(
        &sb,
        &format!(
            "cat /proc/{grandchild}/comm; ps -o sid= -p {grandchild}; ps -o sid= -p {}",
            s.pid()
        ),
    )
    .await;
    assert!(r.stdout.starts_with("sleep"), "{r:?}");
    let t0 = Instant::now();
    let c = sb.cancel_process(&id, Some(200)).await.unwrap();
    println!(
        "PROCESS cancel (grace 200 ms) of a TERM-ignoring setsid grandchild: {:?}",
        t0.elapsed()
    );
    assert!(c.was_running && c.tree_clean, "{c:?}");
    let r = drain(s.output()).await;
    assert!(
        r.exit.cancelled && r.exit.tree_clean && !r.exit.timed_out,
        "{:?}",
        r.exit
    );
    let r = bash(
        &sb,
        &format!("test -e /proc/{grandchild} && echo alive || echo gone"),
    )
    .await;
    assert_eq!(r.stdout.trim(), "gone");

    // A dropped stream (closed connection) stops the tree too.
    let mut s = sb
        .open_process(open_params(&["/bin/sh", "-c", "echo $$; exec sleep 1000"]))
        .await
        .unwrap();
    let Some(ProcessEvent::Stdout(line)) = s.output().next().await.unwrap() else {
        panic!("no pid line");
    };
    let pid: u32 = String::from_utf8_lossy(&line).trim().parse().unwrap();
    drop(s);
    let r = bash(
        &sb,
        &format!("for i in $(seq 50); do test -e /proc/{pid} || break; sleep 0.1; done; test -e /proc/{pid} && echo alive || echo gone"),
    )
    .await;
    assert_eq!(r.stdout.trim(), "gone");

    // Could not start: a typed error, nothing ran.
    for (args, errno) in [
        (vec!["/nonexistent/program"], "ENOENT"),
        (vec!["/etc/passwd"], "EACCES"),
    ] {
        let err = sb.open_process(open_params(&args)).await.unwrap_err();
        assert!(
            matches!(&err, SandboxError::Protocol(ProtocolError::SpawnFailed { errno: e, .. }) if e == errno),
            "{args:?}: {err:?}"
        );
    }
    let mut p = open_params(&["/bin/true"]);
    p.cwd = Some("/nonexistent/dir".into());
    let err = sb.open_process(p).await.unwrap_err();
    assert!(
        matches!(
            &err,
            SandboxError::Protocol(ProtocolError::SpawnFailed { .. })
        ),
        "{err:?}"
    );

    // Timeout, timed in the guest, told apart from a cancel.
    let mut p = open_params(&["/bin/sh", "-c", "sleep 300 & sleep 301"]);
    p.timeout_ms = Some(300);
    p.kill_grace_ms = Some(100);
    let t0 = Instant::now();
    let r = run(&sb, p).await;
    assert!(
        r.exit.timed_out && !r.exit.cancelled && r.exit.tree_clean,
        "{:?}",
        r.exit
    );
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());

    // Output cap: `kill` cuts at the cap, `tail` keeps the end.
    let mut p = open_params(&["/usr/bin/yes"]);
    p.output_cap = Some(OutputCap {
        max_bytes: 100_000,
        on_overflow: Overflow::Kill,
    });
    let r = run(&sb, p).await;
    assert_eq!(r.stdout.len(), 100_000);
    assert!(r.exit.overflow && r.exit.tree_clean, "{:?}", r.exit);
    let mut p = open_params(&["/bin/sh", "-c", "seq 1 100000"]);
    p.output_cap = Some(OutputCap {
        max_bytes: 4096,
        on_overflow: Overflow::Tail,
    });
    let r = run(&sb, p).await;
    assert_eq!(r.stdout.len(), 4096);
    assert!(r.stdout.ends_with(b"\n99999\n100000\n"));
    assert!(r.exit.overflow && r.exit.code == Some(0), "{:?}", r.exit);

    // stderr: tail by default, a stream on request; exit code and signal.
    let r = run(
        &sb,
        open_params(&["/bin/sh", "-c", "echo out; echo err >&2; exit 3"]),
    )
    .await;
    assert_eq!(
        (r.stdout.as_slice(), r.stderr.as_slice()),
        (&b"out\n"[..], &b""[..])
    );
    assert_eq!(
        (r.exit.code, r.exit.stderr_tail.as_str()),
        (Some(3), "err\n")
    );
    let mut p = open_params(&["/bin/sh", "-c", "echo err >&2; kill -KILL $$"]);
    p.stderr = StderrMode::Stream;
    let r = run(&sb, p).await;
    assert_eq!(r.stderr, b"err\n");
    assert_eq!((r.exit.code, r.exit.signal), (None, Some(9)));

    // RLIMIT_NPROC counts this sandbox's processes only (`nproc_scope: sandbox`): 16 is far
    // below what the host user runs outside the guest, and the program still starts.
    let mut p = open_params(&[
        "/bin/sh",
        "-c",
        "grep 'Max processes' /proc/self/limits; ps -e --no-headers | wc -l",
    ]);
    p.limits = Some(ExecLimits {
        nproc: Some(16),
        ..ExecLimits::default()
    });
    let r = run(&sb, p).await;
    let out = String::from_utf8_lossy(&r.stdout).into_owned();
    println!("PROCESS nproc=16 (limit, processes in the guest): {out:?}");
    let limits: Vec<_> = out.split_whitespace().collect();
    assert_eq!(
        limits[..4],
        ["Max", "processes", "16", "16"],
        "{:?}",
        r.exit
    );

    // Warm exec: open → exit of `/bin/true`, one after the other.
    let mut times = Vec::new();
    for _ in 0..100 {
        let t0 = Instant::now();
        let r = run(&sb, open_params(&["/bin/true"])).await;
        times.push(t0.elapsed());
        assert_eq!(r.exit.code, Some(0));
    }
    let (min, max) = (*times.iter().min().unwrap(), *times.iter().max().unwrap());
    println!(
        "PROCESS warm exec /bin/true x100: p50={:?} min={min:?} max={max:?}",
        p50(times)
    );
    let mut times = Vec::new();
    for _ in 0..50 {
        let t0 = Instant::now();
        bash(&sb, "true").await;
        times.push(t0.elapsed());
    }
    println!("PROCESS warm bash tool `true` x50: p50={:?}", p50(times));

    // 32 at once.
    let all = futures_join(
        (0..32)
            .map(|i| {
                let sb = sb.clone();
                async move {
                    let r = run(&sb, open_params(&["/bin/sh", "-c", &format!("echo {i}")])).await;
                    assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), i.to_string());
                }
            })
            .collect(),
    );
    all.await;

    sb.destroy().await.unwrap();
    assert!(leftovers(&env).is_empty());
}

/// Run every future on its own task and wait for all of them.
async fn futures_join<F>(futs: Vec<F>)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    for f in futs {
        set.spawn(f);
    }
    while let Some(r) = set.join_next().await {
        r.unwrap();
    }
}

/// A manager whose mount policy allows `roots`.
fn env_with_roots(roots: Vec<MountRoot>) -> Env {
    let tmp = tempfile::Builder::new()
        .prefix("sclk")
        .tempdir_in("/tmp")
        .unwrap();
    let (cfg, libs) = config(tmp.path(), false, false);
    let backend = Arc::new(LibkrunBackend::new(cfg, libs).unwrap());
    let workspaces = WorkspaceRegistry::open(tmp.path().join("state")).unwrap();
    let templates = TemplateRegistry::load(&[root().join("templates")]).unwrap();
    let manager = SandboxManager::new(
        backend.clone(),
        workspaces,
        templates,
        ManagerConfig {
            mount_policy: Some(MountPolicy::new(roots, Vec::new()).unwrap()),
            ..ManagerConfig::default()
        },
    )
    .unwrap();
    Env {
        project: tmp.path().join("project"),
        docs: tmp.path().join("docs"),
        tmp,
        backend,
        manager,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/base:0.1.0, static crabd"]
async fn host_mounts_at_the_same_path() {
    use std::os::unix::fs::MetadataExt;
    // One root under the temporary directory (`/tmp` is a symlink on macOS: the policy and
    // the paths below use the resolved name) and one in the home directory, whose path the
    // image does not have at all.
    let home = tempfile::Builder::new()
        .prefix("sc-hm-")
        .tempdir_in(std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from))
        .unwrap();
    let home_root = std::fs::canonicalize(home.path()).unwrap();
    let scratch = tempfile::Builder::new()
        .prefix("sc-hm-")
        .tempdir_in("/tmp")
        .unwrap();
    let tmp_root = std::fs::canonicalize(scratch.path()).unwrap();
    let rw = home_root.join("deep/er/rw");
    let ro = home_root.join("ro");
    let under_tmp = tmp_root.join("in-tmp");
    for d in [&rw, &ro, &under_tmp] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("seed.txt"), format!("seed of {}\n", d.display())).unwrap();
    }
    let env = env_with_roots(vec![
        MountRoot {
            path: home_root.clone(),
            max_mode: MountMode::ReadWrite,
        },
        MountRoot {
            path: tmp_root.clone(),
            max_mode: MountMode::ReadWrite,
        },
        MountRoot {
            path: "/usr".into(),
            max_mode: MountMode::ReadOnly,
        },
    ]);
    let caps = env.backend.capabilities();
    assert!(caps.supports_host_mounts && !caps.mount_identity_checked);
    let (rws, ros, tmps) = (
        rw.display().to_string(),
        ro.display().to_string(),
        under_tmp.display().to_string(),
    );
    let host_uid = rustix::process::getuid().as_raw();

    for round in 0..2 {
        let sb = env
            .manager
            .spawn(
                SandboxConfig::from_template("base")
                    .host_mount(&rw, MountMode::ReadWrite)
                    .host_mount(&ro, MountMode::ReadOnly)
                    .host_mount(&under_tmp, MountMode::ReadWrite)
                    .tmp_size_mb(32)
                    .default_cwd(&rw),
            )
            .await
            .unwrap();
        // Same absolute path, virtio-fs, the modes as asked; the root stays the overlay and
        // the read-only rootfs image below it is untouched (round 1 verifies its digest
        // stamp again at spawn).
        let r = bash(
            &sb,
            &format!(
                "pwd; cat seed.txt {ros}/seed.txt {tmps}/seed.txt; \
                 for p in {rws} {ros} {tmps}; do findmnt -no FSTYPE,OPTIONS $p | cut -d, -f1; done; \
                 echo $(findmnt -no FSTYPE /) $(findmnt -no FSTYPE /tmp); \
                 stat -c 'parent %U:%G %a' {}; stat -c 'seed %u:%g' seed.txt; id -u",
                rw.parent().unwrap().display()
            ),
        )
        .await;
        println!("MOUNTS round {round}:\n{}", r.stdout);
        let lines: Vec<_> = r.stdout.lines().collect();
        assert_eq!(lines[0], rws);
        assert_eq!(lines[1], format!("seed of {rws}"));
        assert_eq!(lines[2], format!("seed of {ros}"));
        assert_eq!(lines[3], format!("seed of {tmps}"));
        assert_eq!(lines[4..7], ["virtiofs rw", "virtiofs ro", "virtiofs rw"]);
        assert_eq!(lines[7].trim(), "overlay tmpfs");
        // Mount points the image lacks are root-owned 0755 directories on the scratch layer.
        assert_eq!(lines[8], "parent root:root 755");

        // Guest → host, live; the host file belongs to the host user.
        let r = bash(&sb, &format!("echo from-guest-{round} > out-{round}.txt; mkdir -p d && echo x > d/f; stat -c '%u:%g' out-{round}.txt; echo t > {tmps}/t-{round}")).await;
        assert_eq!(r.exit_code, Some(0), "{r:?}");
        println!(
            "MOUNTS guest-side owner of a file the guest wrote: {}",
            r.stdout.trim()
        );
        let written = rw.join(format!("out-{round}.txt"));
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            format!("from-guest-{round}\n")
        );
        let md = std::fs::metadata(&written).unwrap();
        println!(
            "MOUNTS host-side owner of that file: {}:{} mode {:o} (host uid {host_uid})",
            md.uid(),
            md.gid(),
            md.mode() & 0o7777
        );
        assert_eq!(md.uid(), host_uid);
        assert!(under_tmp.join(format!("t-{round}")).is_file());

        // Host → guest, live, on both.
        std::fs::write(rw.join("late.txt"), format!("late {round}")).unwrap();
        std::fs::write(ro.join("late.txt"), format!("late ro {round}")).unwrap();
        let r = bash(&sb, &format!("cat late.txt {ros}/late.txt")).await;
        assert_eq!(r.stdout, format!("late {round}late ro {round}"));
        // A file the guest has already looked at: a host-side rewrite (same length) and a
        // host-side delete arrive too, once the guest's virtio-fs cache entry has aged out.
        std::fs::write(rw.join("late.txt"), format!("LATE {round}")).unwrap();
        let t0 = Instant::now();
        let seen = bash(
            &sb,
            &format!("for i in $(seq 300); do [ \"$(cat late.txt)\" = 'LATE {round}' ] && break; sleep 0.05; done; cat late.txt"),
        )
        .await;
        assert_eq!(seen.stdout, format!("LATE {round}"));
        println!(
            "MOUNTS host rewrite of a file the guest had read: visible after {:?}",
            t0.elapsed()
        );
        std::fs::remove_file(rw.join("late.txt")).unwrap();
        let t0 = Instant::now();
        let gone = bash(
            &sb,
            "for i in $(seq 300); do test -e late.txt || break; sleep 0.05; done; test -e late.txt; echo $?",
        )
        .await;
        assert_eq!(gone.stdout.trim(), "1");
        println!(
            "MOUNTS host delete of a file the guest had read: visible after {:?}",
            t0.elapsed()
        );

        // ...and a file the guest has just seen missing appears at once when the host
        // creates it (a failed lookup is not cached).
        std::fs::write(rw.join("late.txt"), "back").unwrap();
        assert_eq!(bash(&sb, "cat late.txt").await.stdout, "back");
        std::fs::remove_file(rw.join("late.txt")).unwrap();

        // Read-only holds, also against a remount.
        let r = bash(
            &sb,
            &format!(
                "touch {ros}/x 2>&1; mount -o remount,rw {ros} 2>&1; touch {ros}/y 2>&1; true"
            ),
        )
        .await;
        assert!(r.stdout.contains("Read-only file system"), "{}", r.stdout);
        assert!(!ro.join("x").exists() && !ro.join("y").exists());

        // The system directories around the mount points are intact.
        let r = bash(
            &sb,
            "ls /usr/bin/env /etc/passwd /bin/sh >/dev/null && echo ok",
        )
        .await;
        assert_eq!(r.stdout.trim(), "ok");

        // process.open runs in a host mount by its path.
        let mut p = open_params(&["/bin/sh", "-c", "pwd; cat seed.txt"]);
        p.cwd = Some(ros.clone());
        let r = run(&sb, p).await;
        assert_eq!(
            String::from_utf8_lossy(&r.stdout),
            format!("{ros}\nseed of {ros}\n")
        );
        sb.destroy().await.unwrap();
    }

    // A sandbox user that is not the host user: report what it can do (virtio-fs maps
    // nothing; the helper touches the host files as the host user).
    let sb = env
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .host_mount(&rw, MountMode::ReadWrite)
                .user(UserSpec::new(host_uid + 4242, host_uid + 4242))
                .default_cwd("/tmp"),
        )
        .await
        .unwrap();
    let r = bash(
        &sb,
        &format!("id -u; cat {rws}/seed.txt; echo other > {rws}/other.txt 2>&1; echo write_rc=$?; stat -c '%u:%g %a' {rws}"),
    )
    .await;
    println!("MOUNTS as another uid:\n{}", r.stdout);
    if let Ok(md) = std::fs::metadata(rw.join("other.txt")) {
        println!(
            "MOUNTS host-side owner of its file: {}:{}",
            md.uid(),
            md.gid()
        );
        assert_eq!(md.uid(), host_uid, "the helper writes as the host user");
    }
    sb.destroy().await.unwrap();

    // Refusals: outside every root, a symlink, a system directory of the guest, no policy.
    let link = home_root.join("link");
    std::os::unix::fs::symlink(&rw, &link).unwrap();
    for (path, what) in [
        (PathBuf::from("/opt"), "outside the roots"),
        (link, "symlink"),
        (PathBuf::from("/usr"), "guest system directory"),
    ] {
        let err = env
            .manager
            .spawn(SandboxConfig::from_template("base").host_mount(&path, MountMode::ReadOnly))
            .await
            .err()
            .unwrap_or_else(|| panic!("{what} was mounted"));
        assert!(
            matches!(err, SandboxError::MountNotAllowed { .. }),
            "{what}: {err}"
        );
        println!("MOUNTS refused ({what}): {err}");
    }
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left(), "helper exited");
}

/// Forward target: every connection goes to a TCP listener on the host's loopback.
struct TcpTarget(SocketAddr);

#[async_trait::async_trait]
impl ForwardTarget for TcpTarget {
    async fn connect(
        &self,
        _sandbox: sandcrab_core::SandboxId,
        _guest_port: u16,
    ) -> std::io::Result<BoxedStream> {
        let s = tokio::net::TcpStream::connect(self.0).await?;
        s.set_nodelay(true)?;
        Ok(Box::new(s))
    }
}

/// What the host target saw on one connection.
#[derive(Debug)]
enum Seen {
    /// `up <id>`: bytes received until the guest's end-of-file, and how long they took.
    Up { id: u32, bytes: u64, took: Duration },
    /// `down <n>`: `n` bytes sent, then our end-of-file; the line the guest sent after it.
    Down { ack: String, took: Duration },
}

/// The host target: reads one command line, then
/// - `up <id>`: counts bytes until end-of-file and answers `got <n>` (guest → host
///   half-close: the answer travels after the guest's EOF);
/// - `down <n>`: sends `n` bytes, half-closes, and reads the guest's answer (host → guest
///   half-close).
async fn target_server(
    listener: tokio::net::TcpListener,
    seen: tokio::sync::mpsc::UnboundedSender<Seen>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let seen = seen.clone();
        tokio::spawn(async move {
            let mut s = tokio::io::BufReader::with_capacity(256 * 1024, stream);
            let mut line = String::new();
            s.read_line(&mut line).await.unwrap();
            let f: Vec<_> = line.split_whitespace().collect();
            let t0 = Instant::now();
            match f.as_slice() {
                ["up", id] => {
                    let mut buf = vec![0u8; 256 * 1024];
                    let mut bytes = 0u64;
                    loop {
                        let n = s.read(&mut buf).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        bytes += n as u64;
                    }
                    let took = t0.elapsed();
                    s.get_mut()
                        .write_all(format!("got {bytes}").as_bytes())
                        .await
                        .unwrap();
                    s.get_mut().shutdown().await.unwrap();
                    let _ = seen.send(Seen::Up {
                        id: id.parse().unwrap(),
                        bytes,
                        took,
                    });
                }
                ["down", n] => {
                    let mut left: u64 = n.parse().unwrap();
                    let buf = vec![0xabu8; 256 * 1024];
                    while left > 0 {
                        let n = left.min(buf.len() as u64) as usize;
                        s.get_mut().write_all(&buf[..n]).await.unwrap();
                        left -= n as u64;
                    }
                    s.get_mut().shutdown().await.unwrap();
                    let mut ack = String::new();
                    s.read_to_string(&mut ack).await.unwrap();
                    let _ = seen.send(Seen::Down {
                        ack,
                        took: t0.elapsed(),
                    });
                }
                other => panic!("unexpected command {other:?}"),
            }
        });
    }
}

/// Guest client (node, from the `ts-dev` template): `up <port> <conns> <bytes>` and
/// `down <port> <bytes>`, printing one line per connection.
const FORWARD_CLIENT_JS: &str = r"
const net = require('net');
const [mode, port, a, b] = [process.argv[2], +process.argv[3], +process.argv[4], +process.argv[5]];
function up(id, bytes) {
  return new Promise((resolve, reject) => {
    const s = net.connect({ host: '127.0.0.1', port, allowHalfOpen: true });
    let reply = '';
    s.on('connect', () => {
      s.write(`up ${id}\n`);
      const chunk = Buffer.alloc(65536, id);
      let left = bytes;
      const pump = () => {
        while (left > 0) {
          const c = left >= chunk.length ? chunk : chunk.subarray(0, left);
          left -= c.length;
          if (!s.write(c)) { s.once('drain', pump); return; }
        }
        s.end();
      };
      pump();
    });
    s.on('data', (d) => { reply += d; });
    s.on('end', () => resolve(`up ${id} ${reply}`));
    s.on('error', reject);
  });
}
function down(bytes) {
  return new Promise((resolve, reject) => {
    const s = net.connect({ host: '127.0.0.1', port, allowHalfOpen: true });
    let n = 0;
    const t0 = Date.now();
    s.on('connect', () => s.write(`down ${bytes}\n`));
    s.on('data', (d) => { n += d.length; });
    s.on('end', () => { s.end(`ack ${n}`); resolve(`down ${n} ${Date.now() - t0}ms`); });
    s.on('error', reject);
  });
}
(async () => {
  if (mode === 'up') {
    const all = await Promise.all(Array.from({ length: a }, (_, i) => up(i + 1, b)));
    console.log(all.join('\n'));
  } else if (mode === 'down') {
    console.log(await down(a));
  } else {
    // A port that is not forwarded: nothing listens.
    net.connect({ host: '127.0.0.1', port }).on('error', (e) => console.log(`error ${e.code}`))
      .on('connect', () => console.log('connected'));
  }
})().catch((e) => { console.log(`failed ${e.code || e}`); process.exit(1); });
";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "real libkrun: signed helper, pinned libkrun, pinned sandcrab/ts-dev:0.1.0, static crabd"]
async fn forwards_over_vsock_with_network_disabled() {
    const PORT: u16 = 7801;
    const MIB: u64 = 1024 * 1024;
    let env = env();
    assert!(env.backend.capabilities().supports_forwards);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = TcpTarget(listener.local_addr().unwrap());
    let (tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(target_server(listener, tx));

    let sb = env
        .manager
        .spawn(
            SandboxConfig::from_template("ts-dev")
                .network(NetworkConfig::disabled())
                .forward(PORT)
                .forward(PORT + 1)
                .default_cwd("/tmp"),
        )
        .await
        .unwrap();
    // Forwards bring no network: loopback only, as without them.
    assert_loopback_only(&sb).await;
    sb.call::<FileWriteResult>(FileWriteParams {
        path: "/tmp/fwd.js".into(),
        content: FORWARD_CLIENT_JS.into(),
        append: false,
    })
    .await
    .unwrap();
    let node = |args: String| {
        let sb = sb.clone();
        async move {
            let r = bash_t(&sb, &format!("node /tmp/fwd.js {args}"), Some(120_000)).await;
            assert_eq!(r.exit_code, Some(0), "{r:?}");
            r.stdout
        }
    };

    // Nobody serves the forwards yet: the connection is accepted and closed at once.
    let r = bash_t(
        &sb,
        &format!("node /tmp/fwd.js up {PORT} 1 10"),
        Some(30_000),
    )
    .await;
    assert!(!r.stdout.contains("got"), "{r:?}");
    let stats = sb.forward_stats().await.unwrap();
    let of = |stats: &[sandcrab_protocol::ForwardPortStats], port: u16| {
        *stats.iter().find(|s| s.port == port).unwrap()
    };
    assert_eq!(of(&stats, PORT).refused.no_listener, 1, "{stats:?}");

    sb.serve_forwards(Arc::new(target)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sb.forwards_listening() {
        assert!(
            Instant::now() < deadline,
            "the listen connection never came up"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // One connection: the guest dials 127.0.0.1:<port>, the host target receives, and the
    // answer comes back after the guest's end-of-file.
    let out = node(format!("up {PORT} 1 100000")).await;
    assert_eq!(out.trim(), "up 1 got 100000");
    match seen.recv().await.unwrap() {
        Seen::Up {
            id: 1,
            bytes: 100_000,
            ..
        } => {}
        other => panic!("{other:?}"),
    }

    // 16 at once, 4 MiB each, on the second port.
    let t0 = Instant::now();
    let out = node(format!("up {} 16 {}", PORT + 1, 4 * MIB)).await;
    let took = t0.elapsed();
    let mut ids: Vec<u32> = Vec::new();
    for line in out.lines() {
        let f: Vec<_> = line.split_whitespace().collect();
        assert_eq!(f[2..], ["got", &(4 * MIB).to_string()], "{line}");
        ids.push(f[1].parse().unwrap());
    }
    ids.sort_unstable();
    assert_eq!(ids, (1..=16).collect::<Vec<u32>>());
    for _ in 0..16 {
        assert!(matches!(seen.recv().await.unwrap(), Seen::Up { bytes, .. } if bytes == 4 * MIB));
    }
    println!(
        "FORWARD 16 parallel x 4 MiB guest->host: {took:?} ({:.0} MiB/s total, incl. node start)",
        64.0 / took.as_secs_f64()
    );

    // Throughput of one stream, both ways.
    let size = 512 * MIB;
    node(format!("up {PORT} 1 {size}")).await;
    let Seen::Up { bytes, took, .. } = seen.recv().await.unwrap() else {
        panic!("not an upload");
    };
    assert_eq!(bytes, size);
    let up = size as f64 / 1e6 / took.as_secs_f64();
    println!("FORWARD one stream guest->host: 512 MiB in {took:?} = {up:.0} MB/s");
    let out = node(format!("down {PORT} {size}")).await;
    assert!(out.starts_with(&format!("down {size} ")), "{out}");
    let Seen::Down { ack, took } = seen.recv().await.unwrap() else {
        panic!("not a download");
    };
    // The guest answered after the host's end-of-file: half-close host → guest.
    assert_eq!(ack, format!("ack {size}"));
    let down = size as f64 / 1e6 / took.as_secs_f64();
    println!(
        "FORWARD one stream host->guest: 512 MiB in {took:?} = {down:.0} MB/s (guest saw: {})",
        out.trim()
    );

    // A port that is not forwarded: nothing listens. Still no network.
    let out = node(format!("probe {}", PORT + 2)).await;
    assert_eq!(out.trim(), "error ECONNREFUSED");
    assert_loopback_only(&sb).await;

    let stats = sb.forward_stats().await.unwrap();
    println!("FORWARD stats: {stats:?}");
    assert_eq!(of(&stats, PORT).accepted, 3);
    assert_eq!(of(&stats, PORT + 1).accepted, 16);
    assert_eq!(
        of(&stats, PORT + 1).refused,
        sandcrab_protocol::ForwardRefused::default()
    );

    sb.destroy().await.unwrap();
    server.abort();
    assert!(leftovers(&env).is_empty());
    assert!(no_helper_left(), "helper exited");
}
