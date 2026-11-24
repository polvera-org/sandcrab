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
//! `crabd` comes from `$SANDCRAB_CRABD`, else `<target dir>/<arch>-unknown-linux-musl/release/crabd`
//! (`$CARGO_TARGET_DIR`, relative to the repo root, else `target/`).

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sandcrab_backend_firecracker::{FirecrackerBackend, FirecrackerConfig};
use sandcrab_core::{
    ManagerConfig, MountMode, NetworkConfig, RuntimeBackend, Sandbox, SandboxConfig, SandboxError,
    SandboxManager, TemplateRegistry, WorkspaceRegistry, WorkspaceState,
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
    sandcrab_core::crabd_build_path(&root())
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

/// Serializes the `unrestricted` tests: each test's backend has its own `run_dir`, hence its
/// own tap lock dir, so two of them in parallel would both pick `sctap0` (EBUSY).
static TAP_POOL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let _taps = TAP_POOL.lock().await;
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

/// The egress policy of `scripts/fc-net-setup.sh` with default flags (no `--allow-smtp`,
/// no `--allow-private`), seen from inside a guest: outbound SMTP is refused fast (tcp
/// reset), reserved / private / host ranges are unreachable, HTTPS still works.
/// Host-specific targets that must also be blocked (the host's public IP, the uplink
/// gateway, ...) go in `SANDCRAB_EGRESS_EXTRA_BLOCKED="ip:port ip:port"`, so they are never
/// written into the repo.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires /dev/kvm and `sudo scripts/fc-net-setup.sh` with default egress flags"]
async fn unrestricted_network_egress_policy() {
    let _taps = TAP_POOL.lock().await;
    let env = env();
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base").network(NetworkConfig::unrestricted()))
        .await
        .unwrap();

    // Outbound SMTP / submission to public hosts: refused by our tcp reset, not a timeout.
    // Resolve IPv4 explicitly (the guest has no IPv6 route).
    for (host, port) in [
        ("gmail-smtp-in.l.google.com", 25),
        ("smtp.gmail.com", 465),
        ("smtp.gmail.com", 587),
    ] {
        let cmd = format!(
            "ip=$(getent ahostsv4 {host} | awk 'NR==1{{print $1}}'); echo ip=$ip; \
             timeout 5 bash -c \"</dev/tcp/$ip/{port}\" 2>&1; echo rc=$?"
        );
        let r = bash(&sb, &cmd).await;
        eprintln!(
            "smtp {host}:{port} -> {}",
            r.stdout.trim().replace('\n', " | ")
        );
        assert!(!r.stdout.starts_with("ip=\n"), "DNS for {host}: {r:?}");
        assert!(
            !r.stdout.contains("rc=0"),
            "SMTP {host}:{port} reachable: {}",
            r.stdout
        );
        assert!(
            r.stdout.contains("refused"),
            "SMTP {host}:{port} not reset: {}",
            r.stdout
        );
    }

    // One address per blocked range. A TCP connect that must not succeed (drop → timeout
    // rc=124; the guest kernel may also refuse some outright).
    let mut blocked: Vec<String> = [
        "0.1.2.3:80",         // 0.0.0.0/8
        "192.0.0.8:80",       // 192.0.0.0/24 (IETF protocol assignments)
        "192.0.2.1:80",       // TEST-NET-1
        "198.18.0.1:80",      // benchmarking
        "198.51.100.1:80",    // TEST-NET-2
        "203.0.113.1:80",     // TEST-NET-3
        "240.0.0.1:80",       // class E
        "10.0.0.1:80",        // private
        "172.16.0.1:80",      // private
        "192.168.0.1:80",     // private
        "100.64.0.1:80",      // CGNAT
        "169.254.169.254:80", // cloud metadata
        "172.30.0.1:22",      // tap gateway (guest -> host)
        "172.17.0.1:80",      // docker0 (default bridge)
    ]
    .map(String::from)
    .to_vec();
    if let Ok(extra) = std::env::var("SANDCRAB_EGRESS_EXTRA_BLOCKED") {
        blocked.extend(extra.split_whitespace().map(String::from));
    }
    for t in &blocked {
        let (ip, port) = t.rsplit_once(':').expect("ip:port");
        let cmd = format!("timeout 3 bash -c '</dev/tcp/{ip}/{port}' 2>&1; echo rc=$?");
        let r = bash(&sb, &cmd).await;
        eprintln!("tcp {t} -> {}", r.stdout.trim().replace('\n', " | "));
        assert!(!r.stdout.contains("rc=0"), "{t} reachable: {}", r.stdout);
    }
    // Multicast / limited broadcast are UDP-only destinations. The guest-side send may
    // succeed; the host drops the packet (see the reserved-range rule's counter).
    let r = bash(
        &sb,
        "echo x >/dev/udp/224.0.0.251/5353 2>&1; echo mc=$?; \
         echo x >/dev/udp/255.255.255.255/9 2>&1; echo bc=$?",
    )
    .await;
    eprintln!(
        "udp multicast/broadcast -> {}",
        r.stdout.trim().replace('\n', " | ")
    );

    // Still allowed: HTTPS to the internet.
    let r = bash(
        &sb,
        "curl -4 -sS -o /dev/null -m 15 -w '%{http_code}' https://example.com",
    )
    .await;
    eprintln!("https example.com -> {r:?}");
    assert_eq!(r.exit_code, Some(0), "HTTPS through NAT: {r:?}");
    assert!(r.stdout.starts_with(['2', '3']), "{r:?}");

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
        .spawn(SandboxConfig::from_template("base").port(8080).port(8080))
        .await
        .unwrap_err();
    assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
    assert!(leftovers(&env).is_empty());
    assert!(no_firecracker_left(&env));
}

/// Guest servers for the port test (node, from the `ts-dev` template):
/// - 8080: HTTP (keep-alive, `Connection: close`, HTTP/1.0);
/// - 7001: reads a `<len>\n` header, counts `len` body bytes, replies `got <n>` (upload);
/// - 7002: sends `hello`, half-closes its write side, then records what the client sends
///   afterwards in `/tmp/7002.txt` (guest → host half-close).
const PORT_SERVERS_JS: &str = r"
const net = require('net'), http = require('http'), fs = require('fs');
http.createServer((q, s) => s.end('hello from fc ' + q.url)).listen(8080, '127.0.0.1');
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
}).listen(7002, '127.0.0.1', () => console.log('LISTENING'));
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
            Ok(Ok(0)) | Ok(Err(_)) => return (String::from_utf8_lossy(&got).into_owned(), true),
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

/// Published ports over vsock `CONNECT` + `tunnel.open` on real Firecracker, with network
/// `disabled` (no NIC).
///
/// Must work: HTTP keep-alive, `Connection: close` and HTTP/1.0-with-`SHUT_WR` responses
/// followed by the server's EOF (read-until-close clients), a 4 MiB upload, a refused port,
/// listeners closing on destroy.
///
/// Firecracker's vsock bridge drops half-close in both directions. `crabd` therefore closes a
/// vsock tunnel when its guest target reaches EOF, so the host sees the server's FIN as a full
/// close and later host bytes are dropped (asserted). The host does not forward the client's
/// `SHUT_WR` (Firecracker would turn it into a full close and drop the reply).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires /dev/kvm, pinned artifacts, a pinned sandcrab/ts-dev:0.1.0 and a static crabd"]
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
                .port(7003),
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
        assert_eq!(p.host.ip(), std::net::Ipv4Addr::LOCALHOST, "{p:?}");
    }
    assert_eq!(
        bash(&sb, "ls /sys/class/net").await.stdout.trim(),
        "lo",
        "no NIC"
    );

    // Nothing listens yet: refused per connection (closed without data), the sandbox is fine.
    let (mut r, _w) = TcpStream::connect(host(7003)).await.unwrap().into_split();
    assert_eq!(
        read_until(&mut r, "", Duration::from_secs(10)).await,
        (String::new(), true)
    );

    // `bash` reaps its process group when the call returns: start the servers as a managed
    // background process instead.
    bash(
        &sb,
        &format!("cat > /tmp/s.js <<'EOF'\n{PORT_SERVERS_JS}\nEOF"),
    )
    .await;
    let proc = sb
        .call::<ProcessStartResult>(ProcessStartParams {
            command: "node /tmp/s.js".into(),
            cwd: None,
            env: Default::default(),
            pty: false,
        })
        .await
        .unwrap();
    let mut out = String::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !out.contains("LISTENING") {
        assert!(Instant::now() < deadline, "servers never listened: {out}");
        let p = sb
            .call::<ProcessPollResult>(ProcessPollParams {
                process_id: proc.process_id.clone(),
            })
            .await
            .unwrap();
        out.push_str(&p.stdout);
        out.push_str(&p.stderr);
        assert!(p.running, "servers exited: {p:?} ({out})");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let wait = Duration::from_secs(10);

    // HTTP/1.1 keep-alive: the client reads by Content-Length.
    let (mut r, mut w) = TcpStream::connect(host(8080)).await.unwrap().into_split();
    w.write_all(b"GET /ka HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let (got, _) = read_until(&mut r, "hello from fc /ka", wait).await;
    assert!(got.contains("hello from fc /ka"), "{got:?}");
    // Second request on the same connection.
    w.write_all(b"GET /ka2 HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let (got, _) = read_until(&mut r, "hello from fc /ka2", wait).await;
    assert!(got.contains("hello from fc /ka2"), "{got:?}");
    drop((r, w));

    // `Connection: close`: the full response, then the server's FIN (crabd closes the
    // tunnel on target EOF).
    let (mut r, mut w) = TcpStream::connect(host(8080)).await.unwrap().into_split();
    w.write_all(b"GET /close HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let (got, eof) = read_until(&mut r, "", wait).await;
    println!("PROBE guest FIN reaches host (Connection: close): {eof}");
    assert!(got.contains("hello from fc /close"), "{got:?}");
    assert!(eof, "server FIN not forwarded: {got:?}");
    drop((r, w));

    // HTTP/1.0 with the client half-closing after its request: the reply and the server's
    // FIN still arrive (the host does not forward the SHUT_WR, which Firecracker would make
    // a full close).
    let (mut r, mut w) = TcpStream::connect(host(8080)).await.unwrap().into_split();
    w.write_all(b"GET /half HTTP/1.0\r\n\r\n").await.unwrap();
    w.shutdown().await.unwrap();
    let (got, eof) = read_until(&mut r, "", wait).await;
    assert!(got.contains("hello from fc /half"), "{got:?}");
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

    // Guest half-closes after `hello`: the host gets the bytes and the EOF; crabd closed the
    // tunnel, so later host bytes are dropped (the price of read-until-close on vsock).
    let (mut r, mut w) = TcpStream::connect(host(7002)).await.unwrap().into_split();
    let (got, eof) = read_until(&mut r, "", wait).await;
    assert_eq!((got.as_str(), eof), ("hello", true));
    let _ = w.write_all(b"after-guest-fin").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let seen = bash(&sb, "cat /tmp/7002.txt 2>/dev/null").await.stdout;
    println!("PROBE host bytes after the guest's FIN: guest got {seen:?}");
    assert!(!seen.contains("after-guest-fin"), "{seen:?}");
    drop((r, w));

    sb.destroy().await.unwrap();
    for p in &ports {
        assert!(
            TcpStream::connect(p.host).await.is_err(),
            "{p:?} open after destroy"
        );
    }
    assert!(leftovers(&env).is_empty());
    assert!(no_firecracker_left(&env));
}
