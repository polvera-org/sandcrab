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

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bollard::Docker;
use bollard::query_parameters::ListContainersOptionsBuilder;
use sandcrab_backend_docker::spec::{container_name, relay_name, sock_volume_name};
use sandcrab_backend_docker::{CRABD_ENV, DockerBackend, DockerConfig, DockerTransport, ReapScope};
use sandcrab_core::{
    LifecycleTimings, ManagerConfig, MountMode, NetworkConfig, ResolvedSpec, RuntimeBackend,
    Sandbox, SandboxConfig, SandboxError, SandboxId, SandboxManager, TemplateRegistry,
    WorkspaceRegistry,
};
use sandcrab_protocol::{BashParams, BashResult, FileWriteParams, FileWriteResult};
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

    let spec = resolved_spec(&env, "base");
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
