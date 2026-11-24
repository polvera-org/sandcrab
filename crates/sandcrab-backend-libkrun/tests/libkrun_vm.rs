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
    BashParams, BashResult, FileWriteParams, FileWriteResult, ProcessPollParams, ProcessPollResult,
    ProcessStartParams, ProcessStartResult,
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
    let rel = format!(
        "{}-unknown-linux-musl/release/crabd",
        std::env::consts::ARCH
    );
    if let Some(t) = std::env::var_os("CARGO_TARGET_DIR").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(t).join(&rel);
        if p.is_file() {
            return p;
        }
    }
    root().join("target").join(rel)
}

struct Env {
    tmp: tempfile::TempDir,
    backend: Arc<LibkrunBackend>,
    manager: SandboxManager,
    project: PathBuf,
    docs: PathBuf,
}

fn config(tmp: &Path, tsi: bool) -> (LibkrunConfig, KrunLibs) {
    let home = sandcrab_rootfs::sandcrab_home().unwrap();
    let mut cfg = LibkrunConfig::with_home(&home, crabd());
    cfg.run_dir = tmp.join("run");
    cfg.allow_tsi_unrestricted = tsi;
    let a =
        Artifacts::resolve(&cfg).expect("pinned libkrun artifacts (`sandcrab libkrun install`)");
    let pin = |p: sandcrab_backend_libkrun::artifacts::PinnedPath| PinnedFile {
        path: p.path,
        sha256: p.sha256,
    };
    let libs = KrunLibs {
        libkrun: pin(a.libkrun),
        libkrunfw: pin(a.libkrunfw),
    };
    (cfg, libs)
}

fn env() -> Env {
    env_with(false)
}

fn env_with(tsi: bool) -> Env {
    // Keep the run dir short: socket paths must fit in 104 bytes on macOS.
    let tmp = tempfile::Builder::new()
        .prefix("sclk")
        .tempdir_in("/tmp")
        .unwrap();
    let (cfg, libs) = config(tmp.path(), tsi);
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
fn no_helper_left() -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = std::process::Command::new("pgrep")
            .args(["-x", "-U"])
            .arg(rustix::process::getuid().as_raw().to_string())
            .arg("sandcrab-krun")
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

    // Hardening (guest-hardening step): no_new_privs, no capabilities.
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
    assert!(err.to_string().contains("allow_tsi_unrestricted"), "{err}");
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
