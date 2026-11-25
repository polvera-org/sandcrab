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
    ManagerConfig, MountMode, NetworkConfig, RuntimeBackend, Sandbox, SandboxConfig, SandboxError,
    SandboxManager, TemplateRegistry, TimingMarks, WorkspaceRegistry,
};
use sandcrab_protocol::{
    BashParams, BashResult, FileWriteParams, FileWriteResult, ListDirParams, ListDirResult,
    ProcessPollParams, ProcessPollResult, ProcessStartParams, ProcessStartResult,
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

fn private_env_libs(netstack: bool, tweak_libs: impl FnOnce(&mut KrunLibs)) -> Env {
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
    std::fs::create_dir_all(t.join("bin")).unwrap();
    let helper = t.join("bin").join("sandcrab-krun");
    copy_tree(&cfg.helper, &helper);
    sandcrab_backend_libkrun::verified::HelperRecord::record(&helper).unwrap();
    let dst_rootfs = t.join("cache").join("rootfs");
    std::fs::create_dir_all(&dst_rootfs).unwrap();
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
    env_from(tmp, cfg, libs)
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
