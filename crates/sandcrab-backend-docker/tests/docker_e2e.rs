//! End-to-end tests against a real Docker daemon, the real static `crabd` and the curated
//! template images. All `#[ignore]`d; run with:
//!
//! ```text
//! templates/build.sh base ts-dev       # sandcrab/{base,ts-dev}:0.1.0 present + host-pinned
//! cargo test -p sandcrab-backend-docker -- --ignored --nocapture
//! ```
//!
//! `crabd` comes from `$SANDCRAB_CRABD`, else `target/<arch>-unknown-linux-musl/release/
//! crabd`, which is built with `scripts/build-crabd.sh` on first use if missing.
//! Tests are serialized (one global lock): orphan reaping lists every managed container.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bollard::Docker;
use bollard::query_parameters::ListContainersOptionsBuilder;
use sandcrab_backend_docker::spec::{container_name, relay_name, sock_volume_name};
use sandcrab_backend_docker::{CRABD_ENV, DockerBackend, DockerConfig, DockerTransport, ReapScope};
use sandcrab_core::client::BoxedStream;
use sandcrab_core::{
    ForwardSpec, ForwardTarget, HostMount, LifecycleTimings, ManagerConfig, MountMode, MountPolicy,
    MountRoot, NetworkConfig, NprocScope, ProcessEvent, ProcessOutput, ResolvedSpec,
    ResourceOverrides, RuntimeBackend, Sandbox, SandboxConfig, SandboxError, SandboxFilter,
    SandboxId, SandboxManager, TemplateRegistry, UserSpec, WorkspaceRegistry,
};
use sandcrab_protocol::{
    BashParams, BashResult, FileWriteParams, FileWriteResult, OutputCap, Overflow, ProcessExit,
    ProcessOpenParams, ProtocolError, StderrMode, StdinMode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Path of a static crabd, building it once if needed.
fn crabd() -> &'static Path {
    static CRABD: OnceLock<PathBuf> = OnceLock::new();
    CRABD.get_or_init(|| {
        if let Some(p) = std::env::var_os(CRABD_ENV).filter(|v| !v.is_empty()) {
            return PathBuf::from(p);
        }
        let default = sandcrab_core::crabd_build_path(&root());
        if default.is_file() {
            return default;
        }
        eprintln!("building static crabd via scripts/build-crabd.sh ...");
        let out = Command::new(root().join("scripts/build-crabd.sh"))
            .current_dir(root())
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

struct Env {
    tmp: tempfile::TempDir,
    backend: Arc<DockerBackend>,
    manager: SandboxManager,
    project: PathBuf,
    docs: PathBuf,
    /// Allowed roots for host mounts (canonical): read-write, and read-only at most.
    rw: PathBuf,
    ro: PathBuf,
}

impl Env {
    fn state_dir(&self) -> PathBuf {
        self.tmp.path().join("run")
    }
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let backend = Arc::new(
        DockerBackend::new(DockerConfig {
            crabd_path: crabd().to_path_buf(),
            state_dir: tmp.path().join("run"),
            ..DockerConfig::default()
        })
        .unwrap(),
    );
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
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let (rw, ro) = (base.join("rw"), base.join("ro"));
    std::fs::create_dir_all(&rw).unwrap();
    std::fs::create_dir_all(&ro).unwrap();
    let policy = MountPolicy::new(
        vec![
            MountRoot {
                path: rw.clone(),
                max_mode: MountMode::ReadWrite,
            },
            MountRoot {
                path: ro.clone(),
                max_mode: MountMode::ReadOnly,
            },
        ],
        vec![base.join("run"), base.join("state")],
    )
    .unwrap();
    let manager = SandboxManager::new(
        backend.clone(),
        workspaces,
        templates,
        ManagerConfig {
            mount_policy: Some(policy),
            ..ManagerConfig::default()
        },
    )
    .unwrap();
    Env {
        tmp,
        backend,
        manager,
        project,
        docs,
        rw,
        ro,
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

fn docker() -> Docker {
    Docker::connect_with_defaults().unwrap()
}

async fn container_exists(name: &str) -> bool {
    match docker().inspect_container(name, None).await {
        Ok(_) => true,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => false,
        Err(e) => panic!("inspect {name}: {e}"),
    }
}

async fn assert_gone(env: &Env, id: SandboxId) {
    assert!(
        !container_exists(&container_name(id)).await,
        "container left behind"
    );
    assert!(
        !env.state_dir().join(id.to_string()).exists(),
        "socket dir left behind"
    );
    assert!(
        !container_exists(&relay_name(id)).await,
        "relay left behind"
    );
    assert!(
        docker()
            .inspect_volume(&sock_volume_name(id))
            .await
            .is_err(),
        "socket volume left behind"
    );
}

/// Raw stream to this sandbox's crabd endpoint, bypassing the authenticating connector.
async fn raw_crabd_stream(env: &Env, id: SandboxId) -> Box<dyn sandcrab_core::AsyncStream> {
    match env.backend.transport() {
        DockerTransport::Unix => Box::new(
            tokio::net::UnixStream::connect(
                env.state_dir().join(id.to_string()).join("crabd.sock"),
            )
            .await
            .unwrap(),
        ),
        DockerTransport::Tcp => {
            let info = docker()
                .inspect_container(&relay_name(id), None)
                .await
                .unwrap();
            let bindings = info.network_settings.unwrap().ports.unwrap()["7070/tcp"]
                .clone()
                .unwrap();
            assert!(
                bindings
                    .iter()
                    .all(|b| b.host_ip.as_deref() == Some("127.0.0.1")),
                "relay published beyond loopback: {bindings:?}"
            );
            let port: u16 = bindings[0].host_port.as_deref().unwrap().parse().unwrap();
            Box::new(
                tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .unwrap(),
            )
        }
    }
}

/// Security: on the real endpoint (Unix socket, or the relay's published loopback port),
/// a connection with no token or a wrong token is refused and closed, and no tool call or
/// tunnel runs. The token never reaches `docker inspect` and the token file is consumed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
async fn unauthenticated_connections_are_refused_and_run_nothing() {
    use sandcrab_protocol::{AUTH_TOKEN_BYTES, AuthToken, RpcResponse, codes, encode_frame};

    let _serial = SERIAL.lock().await;
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
    let id = sb.id();
    assert!(
        !env.state_dir()
            .join(id.to_string())
            .join("auth-token")
            .exists(),
        "crabd must consume the token file"
    );
    for name in [container_name(id), relay_name(id)] {
        let Ok(info) = docker().inspect_container(&name, None).await else {
            continue; // no relay with the Unix transport
        };
        // Env, command and labels (the image id is a legitimate 64-hex string elsewhere).
        let config = info.config.unwrap();
        let dump = format!("{:?} {:?} {:?}", config.env, config.cmd, config.labels);
        let hexrun = dump
            .split(|c: char| !c.is_ascii_hexdigit())
            .any(|w| w.len() >= 64);
        assert!(
            !hexrun,
            "{name}: something token-shaped in the container config"
        );
    }

    let touch = |n: u32| {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "bash",
            "params": { "command": format!("touch /workspace/pwned-{n}") }
        })
    };
    let wrong = AuthToken::from_bytes(&[0x42; AUTH_TOKEN_BYTES]);
    for case in 0..3u32 {
        let mut s = raw_crabd_stream(&env, id).await;
        let mut out = Vec::new();
        match case {
            0 => out.extend(encode_frame(&touch(case)).unwrap()),
            1 => {
                out.extend(encode_frame(&wrong.to_request(0)).unwrap());
                out.extend(encode_frame(&touch(case)).unwrap());
            }
            _ => out.extend(
                encode_frame(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tunnel.open", "params": {"port": 22}
                }))
                .unwrap(),
            ),
        }
        s.write_all(&out).await.unwrap();
        let mut hdr = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut hdr))
            .await
            .unwrap()
            .unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(hdr) as usize];
        s.read_exact(&mut body).await.unwrap();
        let resp: RpcResponse = serde_json::from_slice(&body).unwrap();
        assert!(
            matches!(&resp.payload, sandcrab_protocol::ResponsePayload::Error(e) if e.code == codes::UNAUTHORIZED),
            "case {case}: {resp:?}"
        );
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut rest)).await;
        assert!(rest.is_empty(), "case {case}: served after refusal");
    }
    // The authenticated path still works, and none of the refused calls ran.
    let r = bash(&sb, "ls /workspace | grep -c pwned || true").await;
    assert_eq!(r.stdout.trim(), "0", "an unauthenticated call ran");
    assert!(!env.project.join("pwned-0").exists());
    sb.destroy().await.unwrap();
    assert_gone(&env, id).await;
}

/// Can a throwaway container on `network` open a TCP connection to `ip:port`?
fn probe_from_container(network: &str, ip: &str, port: u16) -> bool {
    let out = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--label",
            "sandcrab.test=relay-probe",
            "--network",
            network,
            "--cap-drop",
            "ALL",
            "sandcrab/base:0.1.0",
            "timeout",
            "5",
            "bash",
            "-c",
            &format!("exec 3<>/dev/tcp/{ip}/{port} && echo OPEN"),
        ])
        .output()
        .expect("run docker");
    String::from_utf8_lossy(&out.stdout).contains("OPEN")
}

fn docker_desktop() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.OperatingSystem}}"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("Docker Desktop"))
}

/// Security (review R1): with the `tcp` transport the relay is on its own ICC-off bridge, not
/// the default one, so other containers cannot reach its crabd port: not a container on the
/// relay network, and (Docker Engine) not one on the default bridge. Docker Desktop routes
/// other networks to published ports, so there only the first holds (the token and crabd's
/// pre-auth budget cover the rest).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
async fn relay_is_unreachable_from_other_containers() {
    use sandcrab_backend_docker::spec::{RELAY_NETWORK, RELAY_PORT};

    let _serial = SERIAL.lock().await;
    let env = env();
    if env.backend.transport() != DockerTransport::Tcp {
        println!("SKIP relay_is_unreachable_from_other_containers: unix transport has no relay");
        return;
    }
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap();
    let id = sb.id();
    let relay = docker()
        .inspect_container(&relay_name(id), None)
        .await
        .unwrap();
    let nets = relay.network_settings.unwrap().networks.unwrap();
    assert_eq!(
        nets.keys().collect::<Vec<_>>(),
        [RELAY_NETWORK],
        "relay only on its own network"
    );
    let ip = nets[RELAY_NETWORK].ip_address.clone().unwrap();
    let net = docker().inspect_network(RELAY_NETWORK, None).await.unwrap();
    assert_eq!(
        net.options.unwrap()["com.docker.network.bridge.enable_icc"],
        "false"
    );

    // The host path still works (the sandbox was reached through it to spawn).
    assert_eq!(bash(&sb, "echo ok").await.stdout.trim(), "ok");
    assert!(
        !probe_from_container(RELAY_NETWORK, &ip, RELAY_PORT),
        "a container on the relay network reached the relay"
    );
    let from_bridge = probe_from_container("bridge", &ip, RELAY_PORT);
    if docker_desktop() {
        println!(
            "NOTE Docker Desktop: default-bridge container reaches the relay: {from_bridge} \
             (Desktop routes to published ports across networks)"
        );
    } else {
        assert!(!from_bridge, "a default-bridge container reached the relay");
    }
    sb.destroy().await.unwrap();
    assert_gone(&env, id).await;
}

fn host_ids() -> (u32, u32) {
    (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
async fn base_lifecycle_tools_mounts_and_hardening() {
    let _serial = SERIAL.lock().await;
    let env = env();
    assert!(!env.backend.capabilities().hardware_isolated);
    assert!(!env.backend.capabilities().multi_tenant_safe);
    let sb = env
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .mount("project", "/workspace", MountMode::ReadWrite)
                .mount("docs", "/ref/docs", MountMode::ReadOnly),
        )
        .await
        .unwrap();
    let id = sb.id();
    assert!(container_exists(&container_name(id)).await);

    // bash echo
    let r = bash(&sb, "echo hello").await;
    assert_eq!((r.exit_code, r.stdout.as_str()), (Some(0), "hello\n"));

    // Runs as the host uid:gid.
    let (uid, gid) = host_ids();
    let r = bash(&sb, "echo $(id -u):$(id -g) $PWD $HOME").await;
    assert_eq!(
        r.stdout.trim(),
        format!("{uid}:{gid} /workspace /home/sandcrab")
    );

    // Hardening visible from inside: no capabilities, no_new_privs.
    let r = bash(&sb, "grep -E '^(CapEff|NoNewPrivs):' /proc/self/status").await;
    assert!(
        r.stdout.contains("CapEff:\t0000000000000000"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("NoNewPrivs:\t1"), "{}", r.stdout);

    // rw workspace: guest write visible on host, host-owned.
    let w = sb
        .call::<FileWriteResult>(FileWriteParams {
            path: "out/hello.txt".into(),
            content: "from the crab\n".into(),
            append: false,
        })
        .await
        .unwrap();
    assert_eq!(w.path, "/workspace/out/hello.txt");
    let host_file = env.project.join("out/hello.txt");
    assert_eq!(
        std::fs::read_to_string(&host_file).unwrap(),
        "from the crab\n"
    );
    assert_eq!(std::fs::metadata(&host_file).unwrap().uid(), uid);
    // Host-side mutation is visible to the guest immediately.
    std::fs::write(env.project.join("host.txt"), "from host").unwrap();
    assert_eq!(bash(&sb, "cat host.txt").await.stdout, "from host");

    // ro workspace: kernel-enforced EROFS.
    let err = sb
        .call::<FileWriteResult>(FileWriteParams {
            path: "/ref/docs/x.txt".into(),
            content: "nope".into(),
            append: false,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, SandboxError::Protocol(_)), "{err}");
    let r = bash(&sb, "touch /ref/docs/x.txt 2>&1; cat /ref/docs/README.md").await;
    assert!(r.stdout.contains("Read-only file system"), "{}", r.stdout);
    assert!(r.stdout.contains("read me"));
    assert!(!env.docs.join("x.txt").exists());

    // bash timeout kills the whole process group.
    let started = Instant::now();
    let r = bash_t(&sb, "sleep 300 & sleep 301 & wait", Some(500)).await;
    assert!(r.timed_out, "{r:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    let r = bash(&sb, "pgrep -c -x sleep || true").await;
    assert_eq!(r.stdout.trim(), "0", "sleepers survived the timeout");

    // network `disabled` → network mode `none` (also with the TCP transport's relay), only
    // loopback is up (Docker Desktop's kernel adds down, unaddressed tunnel fallback devices
    // such as `sit0` to every namespace), no routes, no egress.
    let info = docker()
        .inspect_container(&container_name(id), None)
        .await
        .unwrap();
    assert_eq!(
        info.host_config.unwrap().network_mode.as_deref(),
        Some("none")
    );
    let r = bash(
        &sb,
        "for i in /sys/class/net/*; do [ -f $i/flags ] && [ $(( $(cat $i/flags) & 1 )) = 1 ] && basename $i; done; true",
    )
    .await;
    assert_eq!(r.stdout.split_whitespace().collect::<Vec<_>>(), ["lo"]);
    let r = bash(&sb, "tail -n +2 /proc/net/route").await;
    assert_eq!(r.stdout, "", "no IPv4 routes");
    let r = bash(&sb, "curl -sS -m 3 http://1.1.1.1/ >/dev/null").await;
    assert_ne!(r.exit_code, Some(0), "curl succeeded without a network");

    // /tmp is a size-limited tmpfs.
    let r = bash(
        &sb,
        "findmnt -no FSTYPE,OPTIONS /tmp || grep ' /tmp ' /proc/mounts",
    )
    .await;
    assert!(r.stdout.contains("tmpfs"), "{}", r.stdout);
    assert!(r.stdout.contains("noexec"), "{}", r.stdout);
    let r = bash(
        &sb,
        "printf '#!/bin/sh\necho ran\n' > /tmp/x.sh; chmod +x /tmp/x.sh; /tmp/x.sh",
    )
    .await;
    assert_eq!(r.exit_code, Some(126), "/tmp is executable: {r:?}");

    // Core dumps are disabled (soft and hard), so crashes leave no core.* in the workspace.
    let r = bash(&sb, "echo $(ulimit -c) $(ulimit -Hc)").await;
    assert_eq!(r.stdout.trim(), "0 0", "{r:?}");

    let marks = sb.destroy().await.unwrap();
    assert!(marks.teardown().is_some());
    assert!(marks.custom.iter().any(|(n, _)| n == "container_gone"));
    assert_gone(&env, id).await;
    // Idempotent.
    sb.destroy().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/ts-dev:0.1.0 and a static crabd"]
async fn published_port_reachable_from_host() {
    let _serial = SERIAL.lock().await;
    let env = env();
    let sb = env
        .manager
        .spawn(
            SandboxConfig::from_template("ts-dev")
                .network(NetworkConfig::unrestricted())
                .port(8080),
        )
        .await
        .unwrap();
    let ports = sb.published_ports().to_vec();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].guest, 8080);
    assert!(ports[0].host.ip().is_loopback());

    let r = bash(
        &sb,
        "setsid -f node -e \"require('http').createServer((q,r)=>r.end('crab ok')).listen(8080)\" \
         >/dev/null 2>&1 </dev/null; echo started",
    )
    .await;
    assert_eq!(r.exit_code, Some(0), "{r:?}");

    let deadline = Instant::now() + Duration::from_secs(15);
    let body = loop {
        let attempt = async {
            let mut s = tokio::net::TcpStream::connect(ports[0].host).await?;
            s.write_all(b"GET / HTTP/1.0\r\nHost: x\r\n\r\n").await?;
            let mut buf = String::new();
            s.read_to_string(&mut buf).await?;
            std::io::Result::Ok(buf)
        };
        match attempt.await {
            Ok(b) if b.contains("crab ok") => break b,
            _ if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(100)).await,
            other => panic!("published port never answered: {other:?}"),
        }
    };
    assert!(body.starts_with("HTTP/1."), "{body}");

    let id = sb.id();
    sb.destroy().await.unwrap();
    assert_gone(&env, id).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker and a static crabd"]
async fn policy_rejections_leave_nothing_behind() {
    let _serial = SERIAL.lock().await;
    let env = env();

    // Hardware isolation required -> refused before anything is created.
    let err = env
        .manager
        .spawn(SandboxConfig::from_template("base").require_hardware_isolation(true))
        .await
        .unwrap_err();
    assert!(
        matches!(err, SandboxError::HardwareIsolationRequired(_)),
        "{err}"
    );

    let err = env
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .network(NetworkConfig::egress_restricted(["pypi.org"])),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");

    // Backend-level refusal too (not only the manager gate).
    let mut spec = resolved_spec(&env, "base");
    spec.network = NetworkConfig::egress_restricted(["pypi.org"]);
    let err = env.backend.spawn(&spec).await.err().unwrap();
    assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");

    let err = env
        .manager
        .spawn(SandboxConfig::from_template("base").port(8080))
        .await
        .unwrap_err();
    assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
    assert!(err.to_string().contains("unrestricted"), "{err}");

    let leftovers: Vec<_> = std::fs::read_dir(env.state_dir())
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

fn resolved_spec(env: &Env, template: &str) -> ResolvedSpec {
    let template = env.manager.templates().get(template).unwrap().clone();
    let manifest = &template.manifest;
    ResolvedSpec {
        id: SandboxId::new(),
        image: manifest.image().to_owned(),
        // The host-local pin, exactly as the manager resolves it.
        image_sha256: sandcrab_core::effective_pin(
            sandcrab_core::PinStore::open_default().ok().as_ref(),
            manifest,
            sandcrab_core::ImageArch::host(),
        )
        .unwrap()
        .map(|p| p.digest),
        mounts: Vec::new(),
        default_cwd: manifest.workspace.workdir.clone(),
        env: Default::default(),
        network: NetworkConfig::disabled(),
        ports: Vec::new(),
        user: None,
        resources: manifest.resources,
        ttl: None,
        secrets: Vec::new(),
        forwards: Vec::new(),
        host_mounts: Vec::new(),
        tmp_exec: false,
        tmp_size_mb: None,
        pids_max: None,
        nofile: None,
        timings: LifecycleTimings::start(),
        template,
    }
}

async fn container_labels(name: &str) -> HashMap<String, String> {
    docker()
        .inspect_container(name, None)
        .await
        .unwrap()
        .config
        .and_then(|c| c.labels)
        .unwrap_or_default()
}

async fn managed_ids() -> HashSet<SandboxId> {
    let filters = HashMap::from([("label", vec!["sandcrab.managed=true".to_owned()])]);
    docker()
        .list_containers(Some(
            ListContainersOptionsBuilder::new()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await
        .unwrap()
        .into_iter()
        .filter_map(|c| {
            let id = c.labels?.get("sandcrab.sandbox_id")?.parse().ok()?;
            Some(SandboxId(id))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
async fn reap_orphans_removes_only_untracked_managed_containers() {
    let _serial = SERIAL.lock().await;
    let env = env();

    // Pre-existing managed sandboxes (other users of this daemon) count as tracked.
    let mut tracked = managed_ids().await;

    // The orphan carries the server options: they change nothing about reaping.
    let mut spec = resolved_spec(&env, "base");
    spec.forwards = vec![ForwardSpec::new(15001)];
    spec.tmp_exec = true;
    spec.pids_max = Some(128);
    let orphan_id = spec.id;
    drop(env.backend.spawn(&spec).await.unwrap()); // "crash": handle dropped, no destroy

    let kept = env
        .manager
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap();
    tracked.insert(kept.id());

    // An unlabeled container must never be touched.
    let bystander = format!(
        "sandcrab-test-bystander-{}",
        &orphan_id.0.simple().to_string()[..8]
    );
    let st = Command::new("docker")
        .args([
            "create",
            "--name",
            &bystander,
            "sandcrab/base:0.1.0",
            "true",
        ])
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "{}",
        String::from_utf8_lossy(&st.stderr)
    );

    // An orphan of another state dir (another sandcrab home) is not ours to reap.
    let other = self::env();
    let foreign_spec = resolved_spec(&other, "base");
    let foreign_id = foreign_spec.id;
    drop(other.backend.spawn(&foreign_spec).await.unwrap());
    let labels = container_labels(&container_name(foreign_id)).await;
    assert_eq!(
        labels["sandcrab.owner_pid"],
        std::process::id().to_string(),
        "{labels:?}"
    );
    assert_eq!(
        labels["sandcrab.state_dir"],
        sandcrab_backend_docker::state_dir_key(&other.state_dir())
    );

    let report = env
        .backend
        .reap_orphans(&tracked, ReapScope::Owned)
        .await
        .unwrap();
    let reaped: Vec<_> = report
        .containers
        .iter()
        .filter_map(|c| c.sandbox_id)
        .collect();
    assert_eq!(reaped, [orphan_id], "{report:?}");
    assert!(report.kept >= 1, "{report:?}");
    assert_gone(&env, orphan_id).await;
    assert!(container_exists(&container_name(kept.id())).await);
    assert!(container_exists(&container_name(foreign_id)).await);
    assert!(container_exists(&bystander).await);

    // Its own backend (same state dir) reaps it.
    let report = other
        .backend
        .reap_orphans(&tracked, ReapScope::Owned)
        .await
        .unwrap();
    assert!(
        report
            .containers
            .iter()
            .any(|c| c.sandbox_id == Some(foreign_id)),
        "{report:?}"
    );
    assert_gone(&other, foreign_id).await;

    Command::new("docker")
        .args(["rm", "-f", &bystander])
        .output()
        .unwrap();
    kept.destroy().await.unwrap();
    assert_gone(&env, kept.id()).await;
}

fn p50(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// Not the benchmark (see `docs/plans/phase-1-mvp.bench-spec.md`); a quick smoke number.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd; prints timings"]
async fn timing_smoke_base_w0() {
    let _serial = SERIAL.lock().await;
    let env = env();
    let runs: usize = std::env::var("SANDCRAB_TIMING_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let (mut spawn_ready, mut first_call, mut teardown) = (Vec::new(), Vec::new(), Vec::new());
    let mut last = None;
    for i in 0..runs + 2 {
        let sb = env
            .manager
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();
        assert_eq!(bash(&sb, "true").await.exit_code, Some(0));
        let marks = sb.destroy().await.unwrap();
        if i < 2 {
            continue; // warmup
        }
        spawn_ready.push(marks.spawn_ready().unwrap());
        first_call.push(marks.first_call().unwrap());
        teardown.push(marks.teardown().unwrap());
        last = Some(marks);
    }
    let last = last.unwrap();
    println!(
        "docker base W0 n={runs}: spawn_ready p50={:?} (min {:?} max {:?}) | first_call p50={:?} | teardown p50={:?} (min {:?} max {:?})",
        p50(spawn_ready.clone()),
        spawn_ready.iter().min().unwrap(),
        spawn_ready.iter().max().unwrap(),
        p50(first_call),
        p50(teardown.clone()),
        teardown.iter().min().unwrap(),
        teardown.iter().max().unwrap(),
    );
    let phases: Vec<_> = last
        .custom
        .iter()
        .map(|(n, t)| format!("{n}=+{:?}", *t - last.spawn_requested))
        .collect();
    println!("last-run phases: {}", phases.join(" "));
}

/// A third-party image with no shell whose `USER` is not root (`distroless/static-debian12:
/// nonroot`), pinned by multi-arch index digest and pulled when missing.
const EXTERNAL_NOSHELL_IMAGE: &str = "gcr.io/distroless/static-debian12@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab";

/// An external template whose image has no shell: refused until pinned; once pinned, `crabd`
/// (bind-mounted, the pinned entrypoint) serves the file tools without anything from the image.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, network access to pull a public image, and a static crabd"]
async fn external_noshell_template_after_pin() {
    let _serial = SERIAL.lock().await;
    let pull = Command::new("docker")
        .args(["pull", "-q", EXTERNAL_NOSHELL_IMAGE])
        .status()
        .unwrap();
    assert!(pull.success(), "docker pull {EXTERNAL_NOSHELL_IMAGE}");
    let tmp = tempfile::tempdir().unwrap();
    let backend = Arc::new(
        DockerBackend::new(DockerConfig {
            crabd_path: crabd().to_path_buf(),
            state_dir: tmp.path().join("run"),
            ..DockerConfig::default()
        })
        .unwrap(),
    );
    let tdir = tmp.path().join("templates/noshell-ext");
    std::fs::create_dir_all(&tdir).unwrap();
    std::fs::write(
        tdir.join("sandcrab.toml"),
        format!(
            "[template]\nname = \"noshell-ext\"\nversion = \"1.0.0\"\n\
             image = \"{EXTERNAL_NOSHELL_IMAGE}\"\nexternal = true\n\n[tools]\ntiers = [\"core\"]\n"
        ),
    )
    .unwrap();
    let templates = TemplateRegistry::load(&[tmp.path().join("templates")]).unwrap();
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

    let err = manager
        .spawn(SandboxConfig::from_template("noshell-ext"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("UnpinnedImage"), "{err}");
    assert!(err.contains("sandcrab template pin noshell-ext"), "{err}");

    // What `sandcrab template pin` does.
    let t = templates.get("noshell-ext").unwrap();
    let id = local_image_id(t.image());
    pins.adopt(&t.manifest, sandcrab_core::ImageArch::host().unwrap(), &id)
        .unwrap();

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
    let r = sb
        .call::<sandcrab_protocol::FileReadResult>(sandcrab_protocol::FileReadParams {
            path: "/tmp/hello.txt".into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
    assert!(r.content.contains("no shell needed"), "{r:?}");
    let id = sb.id();
    sb.destroy().await.unwrap();
    assert!(!container_exists(&container_name(id)).await);
}

/// `docker image inspect --format {{.Id}}`, as `sandcrab template pin` records it.
fn local_image_id(image: &str) -> String {
    let out = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", image])
        .output()
        .unwrap();
    assert!(out.status.success(), "inspect {image}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

// ---------------------------------------------------------------------------------------
// Process streams, host mounts, forwards and the per-sandbox options (design
// `docs/design/server-and-sdk.md`), against the real `crabd`.
// ---------------------------------------------------------------------------------------

const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Bound on any single wait in these tests, so a broken stream fails instead of hanging.
const STEP: Duration = Duration::from_secs(60);

fn popen(argv: &[&str]) -> ProcessOpenParams {
    ProcessOpenParams::new(
        uuid::Uuid::new_v4().to_string(),
        argv.iter().map(|a| (*a).to_owned()).collect(),
    )
}

/// `bash -c <script>` with a `PATH` and stderr streamed.
fn script(script: &str) -> ProcessOpenParams {
    let mut p = popen(&["/bin/bash", "-c", script]);
    p.env.insert("PATH".into(), GUEST_PATH.into());
    p.stderr = StderrMode::Stream;
    p
}

#[derive(Debug)]
struct Run {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: ProcessExit,
}

impl Run {
    fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    fn err(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Read to the exit, within [`STEP`].
async fn collect(output: &mut ProcessOutput) -> Run {
    tokio::time::timeout(STEP, collect_slow(output))
        .await
        .expect("process did not end")
}

/// [`collect`] without its time bound (the caller sets one).
async fn collect_slow(output: &mut ProcessOutput) -> Run {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let exit = loop {
        match output.next().await.unwrap() {
            Some(ProcessEvent::Stdout(b)) => stdout.extend_from_slice(&b),
            Some(ProcessEvent::Stderr(b)) => stderr.extend_from_slice(&b),
            Some(ProcessEvent::Exit(exit)) => break exit,
            None => panic!("the stream ended without an exit"),
        }
    };
    assert_eq!(output.next().await.unwrap(), None, "the exit is last");
    Run {
        stdout,
        stderr,
        exit,
    }
}

async fn run(sb: &Sandbox, params: ProcessOpenParams) -> Run {
    let mut ps = sb.open_process(params).await.unwrap();
    collect(ps.output()).await
}

/// The conformance cases of `process.open` (design §2.5), over the real transport.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
async fn process_stream_conformance() {
    let _serial = SERIAL.lock().await;
    let env = env();
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap();

    // The exact environment: nothing added, nothing inherited (not even HOME or PATH).
    let mut p = popen(&["/usr/bin/env"]);
    p.env = BTreeMap::from([
        ("A".to_owned(), "1".to_owned()),
        ("B_C".to_owned(), "x y=z".to_owned()),
        ("EMPTY".to_owned(), String::new()),
    ]);
    let r = run(&sb, p).await;
    let mut lines: Vec<_> = r.out().lines().map(str::to_owned).collect();
    lines.sort();
    assert_eq!(lines, ["A=1", "B_C=x y=z", "EMPTY="], "{r:?}");
    assert_eq!((r.exit.code, r.exit.signal), (Some(0), None));
    assert!(r.exit.tree_clean && !r.exit.cancelled && !r.exit.timed_out && !r.exit.overflow);

    // The exact argv: spaces, an empty element and quotes pass through unchanged.
    let r = run(
        &sb,
        popen(&[
            "/bin/sh",
            "-c",
            "printf '[%s]' \"$0\" \"$@\"",
            "a b",
            "",
            "c'd\"e",
            "$HOME",
        ]),
    )
    .await;
    assert_eq!(r.out(), "[a b][][c'd\"e][$HOME]", "{r:?}");
    // The default cwd, and an explicit one.
    assert_eq!(
        run(&sb, popen(&["/bin/pwd"])).await.out(),
        format!("{}\n", sb.default_cwd().display())
    );
    let mut p = popen(&["/bin/pwd"]);
    p.cwd = Some("/usr/lib".into());
    assert_eq!(run(&sb, p).await.out(), "/usr/lib\n");
    // The exit code.
    let r = run(&sb, popen(&["/bin/sh", "-c", "exit 7"])).await;
    assert_eq!(r.exit.code, Some(7));

    // 1 MiB through stdin and back, in order.
    let mut p = popen(&["/bin/cat"]);
    p.stdin = StdinMode::Pipe;
    let (mut stdin, mut output) = sb.open_process(p).await.unwrap().into_split();
    let data: Vec<u8> = (0..1usize << 20)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let sent = data.clone();
    let writer = tokio::spawn(async move {
        stdin.write(&sent).await.unwrap();
        stdin.close().await.unwrap();
        stdin
    });
    let r = collect(&mut output).await;
    drop(writer.await.unwrap());
    assert_eq!(r.stdout.len(), data.len());
    assert!(r.stdout == data, "1 MiB came back out of order");
    assert_eq!(r.exit.code, Some(0));

    // stdin stays open until STDIN_EOF; with `null` it is at end-of-file from the start.
    let mut p = popen(&["/bin/cat"]);
    p.stdin = StdinMode::Pipe;
    let mut ps = sb.open_process(p).await.unwrap();
    ps.stdin().write(b"hi\n").await.unwrap();
    assert_eq!(
        ps.output().next().await.unwrap(),
        Some(ProcessEvent::Stdout(b"hi\n".to_vec()))
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(400), ps.output().next())
            .await
            .is_err(),
        "cat ended while its stdin was open"
    );
    ps.stdin().close().await.unwrap();
    let r = collect(ps.output()).await;
    assert_eq!((r.exit.code, r.stdout.len()), (Some(0), 0));
    let r = run(&sb, popen(&["/bin/cat"])).await;
    assert_eq!((r.exit.code, r.stdout.len()), (Some(0), 0));
    let mut ps = sb.open_process(popen(&["/bin/cat"])).await.unwrap();
    assert!(ps.stdin().write(b"x").await.is_err(), "stdin: null");
    drop(ps);

    // Cancel ends the whole tree: a grandchild in its own session that ignores SIGTERM.
    let p = script("setsid /bin/bash -c 'trap \"\" TERM; echo $$; sleep 300; sleep 300' & wait");
    let id = p.process_id.clone();
    let mut ps = sb.open_process(p).await.unwrap();
    let Some(ProcessEvent::Stdout(line)) = ps.output().next().await.unwrap() else {
        panic!("no pid line");
    };
    let grandchild: u32 = String::from_utf8_lossy(&line).trim().parse().unwrap();
    let alive = run(&sb, script(&format!("kill -0 {grandchild} && echo alive"))).await;
    assert_eq!(alive.out(), "alive\n", "{alive:?}");
    let started = Instant::now();
    let c = sb.cancel_process(&id, Some(200)).await.unwrap();
    assert!(c.was_running && c.tree_clean, "{c:?}");
    let r = collect(ps.output()).await;
    assert!(r.exit.cancelled && r.exit.tree_clean, "{r:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    let r = run(
        &sb,
        script(&format!(
            "kill -0 {grandchild} 2>/dev/null && echo alive; pgrep -c -x sleep; true"
        )),
    )
    .await;
    assert_eq!(r.out(), "0\n", "something of the tree survived the cancel");
    // A cancel of something that is not running says so.
    let c = sb.cancel_process(&id, Some(0)).await.unwrap();
    assert!(!c.was_running, "{c:?}");

    // Closing the connection ends the tree too.
    let mut ps = sb.open_process(script("echo up; sleep 300")).await.unwrap();
    assert!(matches!(
        ps.output().next().await.unwrap(),
        Some(ProcessEvent::Stdout(_))
    ));
    drop(ps);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = run(&sb, script("pgrep -c -x sleep; true")).await;
        if r.out() == "0\n" {
            break;
        }
        assert!(Instant::now() < deadline, "sleep outlived its connection");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // A program that cannot start is a typed error, not a stream.
    let err = sb
        .open_process(popen(&["/no/such/program"]))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            SandboxError::Protocol(ProtocolError::SpawnFailed { errno, .. }) if errno == "ENOENT"
        ),
        "{err:?}"
    );
    let mut p = popen(&["/bin/true"]);
    p.cwd = Some("/no/such/dir".into());
    let err = sb.open_process(p).await.unwrap_err();
    assert!(
        matches!(
            &err,
            SandboxError::Protocol(ProtocolError::SpawnFailed { .. })
        ),
        "{err:?}"
    );

    // The timeout is timed in the guest and stops the tree.
    let mut p = popen(&["/bin/sleep", "300"]);
    p.timeout_ms = Some(300);
    p.kill_grace_ms = Some(100);
    let started = Instant::now();
    let r = run(&sb, p).await;
    assert!(r.exit.timed_out && r.exit.tree_clean, "{r:?}");
    assert_eq!(r.exit.code, None);
    assert!(started.elapsed() < Duration::from_secs(10));

    // Output cap, kill: exactly the cap is streamed, then the tree is stopped.
    let mut p = popen(&["/usr/bin/yes"]);
    p.output_cap = Some(OutputCap {
        max_bytes: 100_000,
        on_overflow: Overflow::Kill,
    });
    p.kill_grace_ms = Some(100);
    let r = run(&sb, p).await;
    assert_eq!(r.stdout.len(), 100_000);
    assert!(r.exit.overflow && r.exit.tree_clean, "{:?}", r.exit);
    // Output cap, tail: nothing is streamed until the end, then the last bytes.
    let mut p = script("printf 0123456789abcdefghij");
    p.output_cap = Some(OutputCap {
        max_bytes: 10,
        on_overflow: Overflow::Tail,
    });
    let r = run(&sb, p).await;
    assert_eq!(r.out(), "abcdefghij");
    assert!(r.exit.overflow, "{:?}", r.exit);
    assert_eq!(r.exit.code, Some(0));

    // stderr: tail (default), merge, stream.
    let both = "echo out; echo err >&2";
    let mut p = script(both);
    p.stderr = StderrMode::Tail;
    let r = run(&sb, p).await;
    assert_eq!((r.out().as_str(), r.stderr.len()), ("out\n", 0));
    assert_eq!(r.exit.stderr_tail, "err\n");
    let mut p = script(both);
    p.stderr = StderrMode::Merge;
    let r = run(&sb, p).await;
    assert_eq!((r.out().as_str(), r.stderr.len()), ("out\nerr\n", 0));
    assert_eq!(r.exit.stderr_tail, "");
    let r = run(&sb, script(both)).await;
    assert_eq!((r.out().as_str(), r.err().as_str()), ("out\n", "err\n"));
    assert_eq!(r.exit.stderr_tail, "err\n");
    // The stream is not bounded by the tail's cap (64 KiB).
    let r = run(&sb, script("head -c 300000 /dev/zero >&2")).await;
    assert_eq!(r.stderr.len(), 300_000);
    assert!(r.exit.stderr_tail.len() <= 65_536);

    let id = sb.id();
    sb.destroy().await.unwrap();
    assert_gone(&env, id).await;
}

/// A sandbox user that is not the server's and has no passwd entry in the image. As root
/// (the server's deployment on Linux) neither the uid nor the gid is shared; otherwise the
/// gid is this process's, which is what lets a non-root server open the socket.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
async fn sandbox_user_differs_from_the_servers() {
    let _serial = SERIAL.lock().await;
    let env = env();
    let (host_uid, host_gid) = host_ids();
    let root = host_uid == 0;
    let user = UserSpec::new(4242, if root { 4243 } else { host_gid });
    println!("server {host_uid}:{host_gid}, sandbox user {user}");

    // Zero mounts.
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base").user(user))
        .await
        .unwrap();
    let r = run(&sb, popen(&["/usr/bin/id", "-u"])).await;
    assert_eq!(r.out(), "4242\n", "{r:?}");
    let r = run(&sb, script("id -g; getent passwd 4242; echo rc=$?")).await;
    assert_eq!(r.out(), format!("{}\nrc=2\n", user.gid), "{r:?}");
    // HOME and the rest come only from the given env.
    let mut p = popen(&["/usr/bin/env"]);
    p.env.insert("HOME".into(), "/nonexistent".into());
    assert_eq!(run(&sb, p).await.out(), "HOME=/nonexistent\n");
    // The tools work too.
    assert_eq!(bash(&sb, "id -u").await.stdout, "4242\n");
    let id = sb.id();
    sb.destroy().await.unwrap();
    assert_gone(&env, id).await;

    // A read-write mount: files are owned by the sandbox user on the host.
    let dir = env.rw.join("out");
    std::fs::create_dir(&dir).unwrap();
    if root {
        std::os::unix::fs::chown(&dir, Some(user.uid), Some(user.gid)).unwrap();
    } else {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
    }
    let sb = env
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .user(user)
                .host_mount(&dir, MountMode::ReadWrite)
                .default_cwd(&dir),
        )
        .await
        .unwrap();
    let r = run(&sb, script("pwd; echo hi > made-inside; id -u")).await;
    assert_eq!(r.out(), format!("{}\n4242\n", dir.display()), "{r:?}");
    let meta = std::fs::metadata(dir.join("made-inside")).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (user.uid, user.gid));
    let id = sb.id();
    sb.destroy().await.unwrap();
    assert_gone(&env, id).await;

    // A user whose socket this process could not open is refused up front.
    if !root && env.backend.transport() == DockerTransport::Unix {
        let err = env
            .manager
            .spawn(SandboxConfig::from_template("base").user(UserSpec::new(4242, 4_000_002)))
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::InvalidConfig(_)), "{err}");
    }
    let leftovers: Vec<_> = std::fs::read_dir(env.state_dir())
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

fn refusal(err: &SandboxError) -> &str {
    match err {
        SandboxError::MountNotAllowed { reason, .. } => reason,
        other => panic!("expected MountNotAllowed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
async fn host_mounts_same_path_modes_and_refusals() {
    let _serial = SERIAL.lock().await;
    let env = env();
    let caps = env.backend.capabilities();
    assert!(caps.supports_host_mounts);
    let (uid, _) = host_ids();
    let out = env.rw.join("a");
    let input = env.ro.join("x");
    for d in [&out, &input, &env.rw.join("a/b"), &env.rw.join("c")] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(input.join("data.txt"), "input\n").unwrap();

    let sb = env
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .host_mount(&out, MountMode::ReadWrite)
                .host_mount(&input, MountMode::ReadOnly),
        )
        .await
        .unwrap();
    let id = sb.id();
    assert_eq!(
        sb.info().host_mounts,
        [
            HostMount::new(&out, MountMode::ReadWrite),
            HostMount::new(&input, MountMode::ReadOnly)
        ]
    );

    // The same path inside, and the kernel's own numbers for it: the mount point is the
    // host directory (design §6.3; what the manager compared before it returned).
    assert_eq!(caps.mount_identity_checked, cfg!(target_os = "linux"));
    let ids = sb
        .fs_identity(vec![
            out.to_str().unwrap().to_owned(),
            input.to_str().unwrap().to_owned(),
        ])
        .await
        .unwrap();
    if caps.mount_identity_checked {
        for (dir, got) in [&out, &input].into_iter().zip(&ids) {
            let m = std::fs::metadata(dir).unwrap();
            assert_eq!((got.dev, got.ino), (m.dev(), m.ino()), "{}", dir.display());
        }
    }

    // rw: a write lands on the host, owned by the sandbox user (this process's uid).
    let o = out.display();
    let i = input.display();
    let r = run(
        &sb,
        script(&format!(
            "cd {o} && echo from-sandbox > made.txt && cat {i}/data.txt"
        )),
    )
    .await;
    assert_eq!(
        (r.exit.code, r.out().as_str()),
        (Some(0), "input\n"),
        "{r:?}"
    );
    let made = out.join("made.txt");
    assert_eq!(std::fs::read_to_string(&made).unwrap(), "from-sandbox\n");
    assert_eq!(std::fs::metadata(&made).unwrap().uid(), uid);
    // ro: the kernel refuses writes.
    let r = run(&sb, script(&format!("touch {i}/nope; echo rc=$?"))).await;
    assert_eq!(r.out(), "rc=1\n", "{r:?}");
    assert!(r.err().contains("Read-only file system"), "{r:?}");
    assert!(!input.join("nope").exists());
    // A change on the host shows up inside at once, in both.
    std::fs::write(out.join("later.txt"), "host-rw\n").unwrap();
    std::fs::write(input.join("later.txt"), "host-ro\n").unwrap();
    let r = run(&sb, script(&format!("cat {o}/later.txt {i}/later.txt"))).await;
    assert_eq!(r.out(), "host-rw\nhost-ro\n", "{r:?}");
    // Nothing else of the host is there: the roots hold only the two mount points.
    let r = run(&sb, script(&format!("ls {}", env.rw.display()))).await;
    assert_eq!(r.out(), "a\n", "{r:?}");

    // Refusals: a symlink, nesting, outside the roots, above the root's mode, the
    // backend's own state directory.
    std::os::unix::fs::symlink(&out, env.rw.join("link")).unwrap();
    let outside = env.tmp.path().join("project");
    let spawn = |mounts: Vec<(PathBuf, MountMode)>| {
        let mut c = SandboxConfig::from_template("base");
        for (p, m) in mounts {
            c = c.host_mount(p, m);
        }
        env.manager.spawn(c)
    };
    use MountMode::{ReadOnly as RO, ReadWrite as RW};
    let e = spawn(vec![(env.rw.join("link"), RO)]).await.unwrap_err();
    assert!(refusal(&e).contains("symlink"), "{e}");
    let e = spawn(vec![(env.rw.join("link/b"), RO)]).await.unwrap_err();
    assert!(refusal(&e).contains("symlink"), "{e}");
    let e = spawn(vec![(out.clone(), RO), (out.join("b"), RO)])
        .await
        .unwrap_err();
    assert!(refusal(&e).contains("nested"), "{e}");
    let e = spawn(vec![(outside, RO)]).await.unwrap_err();
    assert!(refusal(&e).contains("root"), "{e}");
    let e = spawn(vec![(input.clone(), RW)]).await.unwrap_err();
    assert!(refusal(&e).contains("read-only"), "{e}");
    let e = spawn(vec![(env.state_dir(), RO)]).await.unwrap_err();
    refusal(&e);
    let e = spawn(vec![(env.rw.join("missing"), RO)]).await.unwrap_err();
    assert!(refusal(&e).contains("does not exist"), "{e}");
    // Two sandboxes may share and nest folders (no rule across sandboxes).
    let second = spawn(vec![(out.join("b"), RW)]).await.unwrap();
    second.destroy().await.unwrap();
    assert_eq!(
        env.manager.list_active().len(),
        1,
        "a refusal left a sandbox"
    );

    sb.destroy().await.unwrap();
    assert_gone(&env, id).await;

    // A root that is swapped between the check and the start fails the create: the
    // directory is renamed away and another one, then a symlink, takes its name.
    let policy = env.manager.mount_policy().unwrap();
    let swap = env.rw.join("c");
    for symlink in [false, true] {
        let mut spec = resolved_spec(&env, "base");
        spec.host_mounts = policy.resolve(&[HostMount::new(&swap, RW)], &[]).unwrap();
        let moved = env.rw.join(format!("moved-{symlink}"));
        std::fs::rename(&swap, &moved).unwrap();
        if symlink {
            std::os::unix::fs::symlink(&moved, &swap).unwrap();
        } else {
            std::fs::create_dir(&swap).unwrap();
        }
        let err = env.backend.spawn(&spec).await.err().unwrap();
        assert!(
            matches!(err, SandboxError::WorkspaceRootChanged(_)),
            "{err:?}"
        );
        assert_gone(&env, spec.id).await;
        if symlink {
            std::fs::remove_file(&swap).unwrap();
        } else {
            std::fs::remove_dir(&swap).unwrap();
        }
        std::fs::rename(&moved, &swap).unwrap();
    }
    let leftovers: Vec<_> = std::fs::read_dir(env.state_dir())
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

const ECHO_PORT: u16 = 15001;
const SINK_PORT: u16 = 15002;
const SOURCE_PORT: u16 = 15003;
/// Bytes of one throughput stream.
const STREAM_BYTES: u64 = 256 * 1024 * 1024;

/// Host-side targets for the forwards: loopback listeners of this test process.
struct Targets {
    echo: SocketAddr,
    sink: SocketAddr,
    source: SocketAddr,
    /// Refuse every connection (the resolver says no).
    refuse: AtomicBool,
    asked: AtomicU32,
    echo_now: Arc<AtomicU32>,
    echo_peak: Arc<AtomicU32>,
    /// Bytes and nanoseconds (first byte to end-of-file) of the last sink connection.
    sunk: Arc<(AtomicU64, AtomicU64)>,
}

impl Targets {
    async fn start() -> Arc<Self> {
        let bind = || tokio::net::TcpListener::bind("127.0.0.1:0");
        let (echo, sink, source) = (
            bind().await.unwrap(),
            bind().await.unwrap(),
            bind().await.unwrap(),
        );
        let t = Arc::new(Self {
            echo: echo.local_addr().unwrap(),
            sink: sink.local_addr().unwrap(),
            source: source.local_addr().unwrap(),
            refuse: AtomicBool::new(false),
            asked: AtomicU32::new(0),
            echo_now: Arc::default(),
            echo_peak: Arc::default(),
            sunk: Arc::default(),
        });
        let (now, peak) = (t.echo_now.clone(), t.echo_peak.clone());
        tokio::spawn(async move {
            while let Ok((mut s, _)) = echo.accept().await {
                let (now, peak) = (now.clone(), peak.clone());
                tokio::spawn(async move {
                    peak.fetch_max(now.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                    let _ = w.shutdown().await;
                    now.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        let sunk = t.sunk.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = sink.accept().await {
                let sunk = sunk.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 256 * 1024];
                    let (mut total, mut started) = (0u64, None);
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        started.get_or_insert_with(Instant::now);
                        total += n as u64;
                    }
                    let nanos = started.map_or(0, |t: Instant| t.elapsed().as_nanos());
                    sunk.1
                        .store(u64::try_from(nanos).unwrap(), Ordering::SeqCst);
                    sunk.0.store(total, Ordering::SeqCst);
                });
            }
        });
        tokio::spawn(async move {
            while let Ok((mut s, _)) = source.accept().await {
                tokio::spawn(async move {
                    let buf = vec![0x5au8; 256 * 1024];
                    let mut left = STREAM_BYTES;
                    while left > 0 {
                        let n = usize::try_from(left.min(buf.len() as u64)).unwrap();
                        if s.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                        left -= n as u64;
                    }
                    let _ = s.shutdown().await;
                    // Hold the connection until the guest side closes it.
                    let _ = s.read(&mut [0u8; 1]).await;
                });
            }
        });
        t
    }
}

#[async_trait::async_trait]
impl ForwardTarget for Targets {
    async fn connect(&self, _sandbox: SandboxId, guest_port: u16) -> std::io::Result<BoxedStream> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        if self.refuse.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the resolver says no",
            ));
        }
        let addr = match guest_port {
            ECHO_PORT => self.echo,
            SINK_PORT => self.sink,
            SOURCE_PORT => self.source,
            other => panic!("asked for a port that is not a forward: {other}"),
        };
        Ok(Box::new(tokio::net::TcpStream::connect(addr).await?))
    }
}

fn mb_per_s(bytes: u64, nanos: u64) -> f64 {
    bytes as f64 / 1e6 / (nanos as f64 / 1e9)
}

/// Forwards with network `none`: a program in the sandbox dials `127.0.0.1:<port>` and a
/// target on the host answers; nothing else is reachable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd; prints throughput"]
async fn forwards_on_network_none() {
    let _serial = SERIAL.lock().await;
    let env = env();
    assert!(env.backend.capabilities().supports_forwards);
    let targets = Targets::start().await;
    let sb = env
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .forward(ECHO_PORT)
                .forward(SINK_PORT)
                .forward(SOURCE_PORT),
        )
        .await
        .unwrap();
    let id = sb.id();
    let info = docker()
        .inspect_container(&container_name(id), None)
        .await
        .unwrap();
    let hc = info.host_config.unwrap();
    assert_eq!(hc.network_mode.as_deref(), Some("none"));
    assert!(hc.port_bindings.is_none_or(|p| p.is_empty()));

    let stat = |stats: &[sandcrab_protocol::ForwardPortStats], port: u16| {
        *stats.iter().find(|s| s.port == port).unwrap()
    };
    // Reads one line from the echo port, or says that the connection was closed.
    let dial = format!(
        "exec 3<>/dev/tcp/127.0.0.1/{ECHO_PORT} || exit 9; \
         if read -r -t 20 l <&3; then echo \"got $l\"; else echo closed; fi"
    );

    // Nobody serves the forwards yet: accepted and closed at once, counted.
    let r = run(&sb, script(&dial)).await;
    assert_eq!(r.out(), "closed\n", "{r:?}");
    let s = stat(&sb.forward_stats().await.unwrap(), ECHO_PORT);
    assert_eq!((s.accepted, s.refused.no_listener), (0, 1), "{s:?}");

    sb.serve_forwards(targets.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sb.forwards_listening() {
        assert!(Instant::now() < deadline, "the forwards never listened");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // One connection, both ways.
    let r = run(
        &sb,
        script(&format!(
            "exec 3<>/dev/tcp/127.0.0.1/{ECHO_PORT} || exit 9; echo hello >&3; \
             read -r -t 20 l <&3; echo \"got $l\""
        )),
    )
    .await;
    assert_eq!(r.out(), "got hello\n", "{r:?}");

    // 16 at once, all open together, none reset.
    let r = run(
        &sb,
        script(&format!(
            "for i in $(seq 1 16); do ( exec 3<>/dev/tcp/127.0.0.1/{ECHO_PORT} || exit 9; \
             echo \"c$i\" >&3; sleep 1; read -r -t 20 l <&3 || echo \"lost$i\"; echo \"$l\" ) & \
             done; wait"
        )),
    )
    .await;
    let mut got: Vec<_> = r.out().lines().map(str::to_owned).collect();
    got.sort();
    let mut want: Vec<_> = (1..=16).map(|i| format!("c{i}")).collect();
    want.sort();
    assert_eq!(got, want, "{r:?}");
    assert_eq!(r.stderr.len(), 0, "{r:?}");
    assert_eq!(targets.echo_peak.load(Ordering::SeqCst), 16);

    // Throughput of one stream, sandbox to host: upload until end-of-file.
    let mut p = script(&format!(
        "head -c {STREAM_BYTES} /dev/zero > /dev/tcp/127.0.0.1/{SINK_PORT}"
    ));
    p.timeout_ms = Some(300_000);
    let r = tokio::time::timeout(Duration::from_secs(310), async {
        let mut ps = sb.open_process(p).await.unwrap();
        loop {
            if let Some(ProcessEvent::Exit(e)) = ps.output().next().await.unwrap() {
                break e;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(r.code, Some(0), "{r:?}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while targets.sunk.0.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the sink never saw end-of-file");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(targets.sunk.0.load(Ordering::SeqCst), STREAM_BYTES);
    let up = mb_per_s(STREAM_BYTES, targets.sunk.1.load(Ordering::SeqCst));
    // Host to sandbox, timed in the guest.
    let mut p = script(&format!(
        "t0=$(date +%s%N); exec 3<>/dev/tcp/127.0.0.1/{SOURCE_PORT} || exit 9; \
         n=$(cat <&3 | wc -c); echo \"$n $(( $(date +%s%N) - t0 ))\""
    ));
    p.timeout_ms = Some(300_000);
    let mut ps = sb.open_process(p).await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(310), collect_slow(ps.output()))
        .await
        .unwrap();
    let nums: Vec<u64> = r
        .out()
        .split_whitespace()
        .map(|n| n.parse().unwrap())
        .collect();
    assert_eq!(nums[0], STREAM_BYTES, "{r:?}");
    let down = mb_per_s(STREAM_BYTES, nums[1]);
    println!(
        "docker forward throughput, one stream of {} MiB: sandbox->host {up:.0} MB/s, \
         host->sandbox {down:.0} MB/s (target 50 MB/s)",
        STREAM_BYTES / (1024 * 1024)
    );

    // The resolver says no: the program's connection is closed, and it is counted.
    let before = stat(&sb.forward_stats().await.unwrap(), ECHO_PORT);
    assert_eq!(
        (before.accepted, before.refused.rejected),
        (17, 0),
        "{before:?}"
    );
    targets.refuse.store(true, Ordering::SeqCst);
    let asked = targets.asked.load(Ordering::SeqCst);
    let r = run(&sb, script(&dial)).await;
    assert_eq!(r.out(), "closed\n", "{r:?}");
    assert_eq!(targets.asked.load(Ordering::SeqCst), asked + 1);
    let after = stat(&sb.forward_stats().await.unwrap(), ECHO_PORT);
    assert_eq!(
        (
            after.accepted,
            after.refused.rejected,
            after.refused.no_listener
        ),
        (17, 1, 1),
        "{after:?}"
    );
    targets.refuse.store(false, Ordering::SeqCst);

    // Nothing else: a port that is not a forward is refused by the kernel, and there is
    // no route to anywhere.
    let r = run(&sb, script("exec 3<>/dev/tcp/127.0.0.1/15999; echo rc=$?")).await;
    assert!(r.err().contains("Connection refused"), "{r:?}");
    let r = run(
        &sb,
        script(
            "curl -sS -m 3 http://1.1.1.1/ >/dev/null 2>&1; echo rc=$?; tail -n +2 /proc/net/route",
        ),
    )
    .await;
    assert!(r.out().starts_with("rc=") && r.out() != "rc=0\n", "{r:?}");
    assert_eq!(r.out().lines().count(), 1, "the sandbox has a route: {r:?}");

    // The forwards end with the sandbox.
    sb.destroy().await.unwrap();
    assert!(!sb.forwards_listening());
    assert_gone(&env, id).await;
}

/// `/tmp` options, the pids limit and the nofile limit reach the container.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
async fn tmp_exec_size_pids_and_nofile() {
    let _serial = SERIAL.lock().await;
    let env = env();
    let caps = env.backend.capabilities();
    assert!(caps.supports_tmp_options && caps.supports_process_limits);
    assert_eq!(caps.nproc_scope, NprocScope::HostUid);
    let sb = env
        .manager
        .spawn(
            SandboxConfig::from_template("base")
                .tmp_exec(true)
                .tmp_size_mb(32)
                .resources(ResourceOverrides {
                    pids_max: Some(96),
                    nofile: Some(512),
                    ..ResourceOverrides::default()
                }),
        )
        .await
        .unwrap();
    let id = sb.id();

    // /tmp runs a binary, and is otherwise as before.
    let r = run(
        &sb,
        script("cp /bin/true /tmp/t && /tmp/t && echo ran; grep ' /tmp ' /proc/mounts"),
    )
    .await;
    let out = r.out();
    assert!(out.starts_with("ran\n"), "{r:?}");
    assert!(out.contains("tmpfs") && out.contains("nosuid") && out.contains("nodev"));
    assert!(!out.contains("noexec"), "{out}");
    assert!(out.contains("size=32768k"), "{out}");
    // nofile, soft and hard; pids.
    let r = run(&sb, script("ulimit -Sn; ulimit -Hn")).await;
    assert_eq!(r.out(), "512\n512\n");
    let info = docker()
        .inspect_container(&container_name(id), None)
        .await
        .unwrap();
    assert_eq!(info.host_config.unwrap().pids_limit, Some(96));
    let r = run(&sb, script("cat /sys/fs/cgroup/pids.max 2>/dev/null; true")).await;
    assert!(r.out().is_empty() || r.out() == "96\n", "{r:?}");
    // Hardening is as without the options.
    let r = run(
        &sb,
        script("grep -E '^(CapEff|NoNewPrivs):' /proc/self/status"),
    )
    .await;
    assert!(r.out().contains("CapEff:\t0000000000000000"), "{r:?}");
    assert!(r.out().contains("NoNewPrivs:\t1"), "{r:?}");
    sb.destroy().await.unwrap();
    assert_gone(&env, id).await;

    // Without `tmp_exec`, /tmp still refuses to run anything.
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap();
    let r = run(&sb, script("cp /bin/true /tmp/t && /tmp/t; echo rc=$?")).await;
    assert_eq!(r.out(), "rc=126\n", "{r:?}");
    let id = sb.id();
    sb.destroy().await.unwrap();
    assert_gone(&env, id).await;
}

/// Labels live in the manager (never on the container); destroy by label confirms.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
async fn labels_and_destroy_by_label() {
    let _serial = SERIAL.lock().await;
    let env = env();
    let spawn = |run: &'static str| {
        env.manager.spawn(
            SandboxConfig::from_template("base")
                .label("run", run)
                .label("kind", "check"),
        )
    };
    let (a, b, c) = tokio::join!(spawn("r1"), spawn("r1"), spawn("r2"));
    let (a, b, c) = (a.unwrap(), b.unwrap(), c.unwrap());
    let by = |run: &str| SandboxFilter {
        labels: BTreeMap::from([("run".to_owned(), run.to_owned())]),
        ..SandboxFilter::default()
    };
    let listed: HashSet<_> = env.manager.list(&by("r1")).iter().map(|i| i.id).collect();
    assert_eq!(listed, HashSet::from([a.id(), b.id()]));
    assert_eq!(env.manager.list(&by("r2"))[0].labels["kind"], "check");
    let on_container = container_labels(&container_name(a.id())).await;
    assert!(!on_container.contains_key("run"), "{on_container:?}");
    assert_eq!(on_container["sandcrab.sandbox_id"], a.id().to_string());

    // A process is running in one of them: destroy stops it first.
    let mut ps = a.open_process(popen(&["/bin/sleep", "300"])).await.unwrap();
    let r = env
        .manager
        .destroy_by_label(&by("r1").labels)
        .await
        .unwrap();
    assert_eq!((r.matched, r.destroyed, r.empty), (2, 2, true), "{r:?}");
    assert!(r.remaining.is_empty());
    // Its stream ends: with an exit if `crabd` got to send one, else as lost.
    tokio::time::timeout(STEP, async {
        while let Ok(Some(e)) = ps.output().next().await {
            if matches!(e, ProcessEvent::Exit(_)) {
                break;
            }
        }
    })
    .await
    .expect("the process outlived its sandbox");
    assert_gone(&env, a.id()).await;
    assert_gone(&env, b.id()).await;
    assert!(container_exists(&container_name(c.id())).await);
    assert_eq!(env.manager.list_active().len(), 1);
    let r = env
        .manager
        .destroy_by_label(&by("r1").labels)
        .await
        .unwrap();
    assert_eq!((r.matched, r.empty), (0, true));

    c.destroy().await.unwrap();
    assert_gone(&env, c.id()).await;
}

/// Sequential `process.open ["/bin/true"]` on a running sandbox, timed here: open, read to
/// the exit. Target: p50 at or under 30 ms (design §13).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd; prints timings"]
async fn timing_warm_exec() {
    let _serial = SERIAL.lock().await;
    let env = env();
    let sb = env
        .manager
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap();
    let runs: usize = std::env::var("SANDCRAB_TIMING_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let mut times = Vec::with_capacity(runs);
    for i in 0..runs + 10 {
        let started = Instant::now();
        let r = run(&sb, popen(&["/bin/true"])).await;
        let took = started.elapsed();
        assert_eq!(r.exit.code, Some(0));
        if i >= 10 {
            times.push(took);
        }
    }
    times.sort();
    println!(
        "docker warm exec [\"/bin/true\"] n={runs}: p50={:?} p90={:?} p99={:?} min={:?} max={:?} \
         (target p50 <= 30ms)",
        times[runs / 2],
        times[runs * 9 / 10],
        times[runs * 99 / 100],
        times[0],
        times[runs - 1],
    );
    let id = sb.id();
    sb.destroy().await.unwrap();
    assert_gone(&env, id).await;
}
