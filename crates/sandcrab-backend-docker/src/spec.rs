//! Pure translation of a [`ResolvedSpec`] into a hardened Docker container definition.
//!
//! Nothing here talks to Docker, so the whole hardening / mount / policy matrix is
//! unit-tested without a daemon.
//!
//! Hardening applied to every container:
//! - `cap_drop: ALL`, `no-new-privileges`, never privileged, private IPC namespace;
//! - `init: true` (docker-init as PID 1 reaps zombies and forwards SIGTERM to `crabd`);
//! - `pids_limit`, `memory` with `memory_swap == memory` (no swap), `nano_cpus`;
//! - size-limited `nosuid,nodev,noexec` tmpfs at `/tmp` (stated explicitly, not left to
//!   Docker's tmpfs default): scripts and shared objects under `/tmp` cannot be executed or
//!   mapped. Tools that unpack and run binaries from a temp dir should get
//!   `TMPDIR=$HOME/.tmp` (or a dir under the workspace) instead;
//! - core dumps disabled (`RLIMIT_CORE` soft = hard = 0): a crashing process must not
//!   drop `core.*` files into the cwd, which is usually a persistent workspace;
//! - no `sysctls`: the hardening sysctls `crabd init` sets in the microVM guests
//!   (`sandcrab_protocol::init::HARDENING_SYSCTLS`) are not network/IPC-namespaced, so
//!   Docker refuses them per container and they stay host-level. Docker's own defaults cover
//!   the same ground: its seccomp profile (no `unshare`/`clone` of namespaces, `bpf`,
//!   `userfaultfd`, `perf_event_open`, `io_uring` without capabilities), a read-only `/sys`
//!   and read-only `/proc/sys` (`ReadonlyPaths`). See `docs/design/guest-seccomp.md`;
//! - runs as the **host** uid:gid unless the spec overrides it, so files written to
//!   bind-mounted workspaces stay editable on the host;
//! - network `none` unless the policy is `unrestricted`; published ports bind to
//!   `127.0.0.1` only;
//! - `crabd` requires a per-sandbox auth token on every connection
//!   (`--auth-token-file`, read once and deleted by `crabd`).
//!
//! With [`DockerTransport::Tcp`] a second, **relay** container ([`build_relay_spec`]) is
//! added: `crabd` then listens on a Unix socket in a per-sandbox tmpfs volume
//! ([`GUEST_SOCK_DIR`]) that both containers mount, and the relay (`crabd relay`, read-only
//! rootfs, no capabilities, tiny limits) publishes [`RELAY_PORT`] on host `127.0.0.1` only.
//! The sandbox container's network mode is the same as with the Unix transport, so
//! `disabled` keeps meaning `none`: the sandbox has no interface besides loopback. Only the
//! relay, which runs no user code, has a network interface, and it is **not** on the default
//! bridge: every relay joins the dedicated [`RELAY_NETWORK`] bridge, created by the backend
//! with inter-container traffic off (`enable_icc=false`) and no outbound NAT. So no other
//! container can reach a relay's port 7070 through the bridge: not another relay on the same
//! network (ICC off), and on Docker Engine not a container on any other network either
//! (Docker isolates bridge networks from each other). Docker Desktop is weaker: it routes a
//! container's traffic to another network's *published* ports (and `host.docker.internal`
//! reaches the host's published ports by design), so there other containers can still open
//! a connection; `crabd`'s token and its pre-auth budget (`sandcrab_guest::preauth`) are
//! what stop them.
//!
//! The root filesystem stays writable (`readonly_rootfs: false`): templates expect a
//! writable `HOME` (`/home/sandcrab`, 1777) and package caches; the container layer is
//! discarded on destroy. Template `disk_mb` is **not enforced** (overlay2 needs an
//! xfs+pquota backing fs for `storage_opt.size`).

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use bollard::models::{
    ContainerCreateBody, HostConfig, Mount, MountBindOptions, MountBindOptionsPropagationEnum,
    MountType, PortBinding, ResourcesUlimits,
};
use sandcrab_core::{
    NetworkConfig, NetworkPolicy, ResolvedSpec, Result, SandboxError, SandboxId, UserSpec,
    WorkspaceBacking, validate_mount_target,
};

use crate::{DockerLimits, DockerTransport, Owner};

/// Where the host `crabd` binary is bind-mounted (read-only) inside the container.
pub const GUEST_CRABD: &str = "/opt/sandcrab/crabd";
/// Guest runtime directory, backed by the per-sandbox host socket directory.
pub const GUEST_RUN_DIR: &str = "/run/sandcrab";
/// File name of the `crabd` Unix socket inside the run directory.
pub const SOCKET_NAME: &str = "crabd.sock";
/// File name of the per-sandbox auth token in the run directory (`crabd` deletes it).
pub const TOKEN_NAME: &str = "auth-token";
/// TCP transport: guest mount point of the shared per-sandbox tmpfs volume holding the
/// `crabd` socket (under the reserved `/opt/sandcrab`, so no workspace can shadow it).
pub const GUEST_SOCK_DIR: &str = "/opt/sandcrab/sock";
/// TCP transport: port the relay listens on inside its container.
pub const RELAY_PORT: u16 = 7070;
/// TCP transport: the user-defined bridge network every relay container joins (see the
/// module docs). Created on first use by the backend; never shared with sandboxes.
pub const RELAY_NETWORK: &str = "sandcrab-relay";
/// Bridge driver options [`RELAY_NETWORK`] must carry: no inter-container traffic and no
/// outbound NAT (the relay only ever talks to a Unix socket).
pub const RELAY_NETWORK_OPTIONS: [(&str, &str); 2] = [
    ("com.docker.network.bridge.enable_icc", "false"),
    ("com.docker.network.bridge.enable_ip_masquerade", "false"),
];
/// TCP transport: size of the socket volume (it only ever holds one socket).
const SOCK_VOLUME_SIZE: &str = "1m";
/// Relay container limits.
const RELAY_PIDS_LIMIT: i64 = 64;
const RELAY_MEMORY: i64 = 64 * 1024 * 1024;
const RELAY_NANO_CPUS: i64 = 1_000_000_000;
/// Default `HOME` (the templates' world-writable home).
pub const DEFAULT_HOME: &str = "/home/sandcrab";

pub const LABEL_MANAGED: &str = "sandcrab.managed";
pub const LABEL_SANDBOX_ID: &str = "sandcrab.sandbox_id";
pub const LABEL_TEMPLATE: &str = "sandcrab.template";
pub const LABEL_BACKEND: &str = "sandcrab.backend";
/// Value of [`LABEL_BACKEND`].
pub const BACKEND_LABEL_VALUE: &str = "docker";
/// [`LABEL_BACKEND`] value written before the backend was renamed from `DockerDev`;
/// containers carrying it are still recognized (and reaped) as Sandcrab-managed.
pub const LEGACY_BACKEND_LABEL_VALUE: &str = "docker-dev";
/// Owner labels (see [`crate::Owner`]): host uid, pid and boot id of the spawning process,
/// and a hash of its socket state directory.
pub const LABEL_OWNER_UID: &str = "sandcrab.owner_uid";
pub const LABEL_OWNER_PID: &str = "sandcrab.owner_pid";
pub const LABEL_OWNER_BOOT_ID: &str = "sandcrab.owner_boot_id";
pub const LABEL_STATE_DIR: &str = "sandcrab.state_dir";
/// `sandbox` or `relay` (TCP transport); absent on containers from before the relay.
pub const LABEL_ROLE: &str = "sandcrab.role";
pub const ROLE_SANDBOX: &str = "sandbox";
pub const ROLE_RELAY: &str = "relay";

/// Host-side facts the container definition depends on.
#[derive(Debug, Clone, Copy)]
pub struct SpecContext<'a> {
    /// Canonical host path of the static `crabd` binary.
    pub crabd_path: &'a Path,
    /// Host per-sandbox socket directory (mounted at [`GUEST_RUN_DIR`]).
    pub socket_dir: &'a Path,
    /// Image reference to run (the verified local image ID).
    pub image: &'a str,
    /// uid:gid of the host process; the default container user.
    pub host_user: UserSpec,
    pub limits: &'a DockerLimits,
    /// The spawning process, recorded as owner labels for scoped orphan reaping.
    pub owner: &'a Owner,
    /// Host ↔ `crabd` transport.
    pub transport: DockerTransport,
}

/// Docker `network_mode` for a policy. `egress_restricted` cannot be enforced by plain
/// Docker networking and is refused, never downgraded.
pub fn network_mode(network: &NetworkConfig) -> Result<&'static str> {
    match network.policy {
        NetworkPolicy::Disabled => Ok("none"),
        NetworkPolicy::Unrestricted => Ok("bridge"),
        NetworkPolicy::EgressRestricted => Err(SandboxError::PolicyUnenforceable(
            "docker cannot enforce network policy `egress_restricted` (no domain \
             allow-list support); use `disabled` or `unrestricted`"
                .into(),
        )),
    }
}

/// `sandcrab-<first 12 hex chars of the sandbox id>`.
#[must_use]
pub fn container_name(id: SandboxId) -> String {
    let hex = id.0.simple().to_string();
    format!("sandcrab-{}", &hex[..12])
}

/// `sandcrab-<12 hex>-relay`: the TCP transport's relay container.
#[must_use]
pub fn relay_name(id: SandboxId) -> String {
    format!("{}-relay", container_name(id))
}

/// `sandcrab-<12 hex>-sock`: the TCP transport's socket volume.
#[must_use]
pub fn sock_volume_name(id: SandboxId) -> String {
    format!("{}-sock", container_name(id))
}

/// `crabd`'s socket path inside the sandbox container.
#[must_use]
pub fn guest_socket(transport: DockerTransport) -> String {
    match transport {
        DockerTransport::Unix => format!("{GUEST_RUN_DIR}/{SOCKET_NAME}"),
        DockerTransport::Tcp => format!("{GUEST_SOCK_DIR}/{SOCKET_NAME}"),
    }
}

/// Labels shared by every Docker object of a sandbox.
fn base_labels(spec: &ResolvedSpec, owner: &Owner, role: &str) -> HashMap<String, String> {
    let mut labels = HashMap::from([
        (LABEL_MANAGED.to_owned(), "true".to_owned()),
        (LABEL_SANDBOX_ID.to_owned(), spec.id.to_string()),
        (LABEL_TEMPLATE.to_owned(), spec.template.name().to_owned()),
        (LABEL_BACKEND.to_owned(), BACKEND_LABEL_VALUE.to_owned()),
        (LABEL_ROLE.to_owned(), role.to_owned()),
    ]);
    labels.extend(owner.labels().into_iter().map(|(k, v)| (k.to_owned(), v)));
    labels
}

/// `local`-driver tmpfs volume for the TCP transport's socket: tiny,
/// `nosuid,nodev,noexec`, owned by and private (0700) to the container user.
#[must_use]
pub fn sock_volume_spec(
    spec: &ResolvedSpec,
    host_user: UserSpec,
    owner: &Owner,
) -> bollard::models::VolumeCreateRequest {
    let user = effective_user(spec, host_user);
    bollard::models::VolumeCreateRequest {
        name: Some(sock_volume_name(spec.id)),
        driver: Some("local".into()),
        driver_opts: Some(HashMap::from([
            ("type".to_owned(), "tmpfs".to_owned()),
            ("device".to_owned(), "tmpfs".to_owned()),
            (
                "o".to_owned(),
                format!(
                    "size={SOCK_VOLUME_SIZE},mode=0700,uid={},gid={},nosuid,nodev,noexec",
                    user.uid, user.gid
                ),
            ),
        ])),
        labels: Some(base_labels(spec, owner, ROLE_SANDBOX)),
        ..Default::default()
    }
}

fn volume(name: &str, target: &str, read_only: bool) -> Mount {
    Mount {
        target: Some(target.to_owned()),
        source: Some(name.to_owned()),
        typ: Some(MountType::VOLUME),
        read_only: Some(read_only),
        ..Default::default()
    }
}

fn loopback_binding(port_key: &str) -> HashMap<String, Option<Vec<PortBinding>>> {
    HashMap::from([(
        port_key.to_owned(),
        Some(vec![PortBinding {
            host_ip: Some("127.0.0.1".into()),
            host_port: Some(String::new()),
        }]),
    )])
}

/// Container user: the spec override, else the host uid:gid.
#[must_use]
pub fn effective_user(spec: &ResolvedSpec, host_user: UserSpec) -> UserSpec {
    spec.user.unwrap_or(host_user)
}

fn bind(source: &Path, target: &str, read_only: bool) -> Result<Mount> {
    let source = source.to_str().ok_or_else(|| {
        SandboxError::InvalidConfig(format!(
            "host path `{}` is not valid UTF-8",
            source.display()
        ))
    })?;
    Ok(Mount {
        target: Some(target.to_owned()),
        source: Some(source.to_owned()),
        typ: Some(MountType::BIND),
        read_only: Some(read_only),
        bind_options: Some(MountBindOptions {
            propagation: Some(MountBindOptionsPropagationEnum::RPRIVATE),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn guest_str(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        SandboxError::InvalidConfig(format!("guest path `{}` is not UTF-8", path.display()))
    })
}

/// Build the full container definition for `spec`.
pub fn build_container_spec(
    spec: &ResolvedSpec,
    ctx: &SpecContext<'_>,
) -> Result<ContainerCreateBody> {
    spec.reject_secrets()?;
    let mode = network_mode(&spec.network)?;

    // Ports: loopback-only, ephemeral host port.
    let mut guest_ports = BTreeSet::new();
    for p in &spec.ports {
        if p.guest == 0 {
            return Err(SandboxError::InvalidConfig(
                "guest port 0 is not publishable".into(),
            ));
        }
        if !guest_ports.insert(p.guest) {
            return Err(SandboxError::InvalidConfig(format!(
                "guest port {} is listed twice",
                p.guest
            )));
        }
    }
    if !guest_ports.is_empty() && mode == "none" {
        return Err(SandboxError::InvalidConfig(format!(
            "publishing ports {guest_ports:?} needs network policy `unrestricted`: with \
             `{}` the container has no network interface to publish from",
            spec.network.policy
        )));
    }
    let port_keys: Vec<String> = guest_ports.iter().map(|p| format!("{p}/tcp")).collect();
    let port_bindings: HashMap<String, Option<Vec<PortBinding>>> =
        port_keys.iter().flat_map(|k| loopback_binding(k)).collect();

    // Mounts: crabd (ro), run dir (rw: token, and the socket with the Unix transport),
    // [TCP: socket volume (rw)], then workspaces (ro honored by the kernel).
    let mut mounts = vec![
        bind(ctx.crabd_path, GUEST_CRABD, true)?,
        bind(ctx.socket_dir, GUEST_RUN_DIR, false)?,
    ];
    if ctx.transport == DockerTransport::Tcp {
        mounts.push(volume(&sock_volume_name(spec.id), GUEST_SOCK_DIR, false));
    }
    for m in &spec.mounts {
        if let WorkspaceBacking::BlockVolume(_) = m.backing {
            return Err(SandboxError::InvalidConfig(format!(
                "workspace `{}` is a block volume; docker only mounts directories",
                m.workspace_name
            )));
        }
        if !m.host_path.is_absolute() {
            return Err(SandboxError::InvalidConfig(format!(
                "workspace `{}` host path `{}` is not absolute",
                m.workspace_name,
                m.host_path.display()
            )));
        }
        let target = validate_mount_target(&m.guest_path)?;
        mounts.push(bind(
            &m.host_path,
            guest_str(&target)?,
            !m.mode.is_writable(),
        )?);
    }

    // Environment.
    let mut env = Vec::with_capacity(spec.env.len() + 1);
    for (k, v) in &spec.env {
        if k.is_empty() || k.contains('=') || k.contains('\0') || v.contains('\0') {
            return Err(SandboxError::InvalidConfig(format!(
                "invalid environment variable name `{k}`"
            )));
        }
        env.push(format!("{k}={v}"));
    }
    if !spec.env.contains_key("HOME") {
        env.push(format!("HOME={DEFAULT_HOME}"));
    }

    // Resources.
    let r = &spec.resources;
    let memory = r
        .memory_mb
        .checked_mul(1024 * 1024)
        .and_then(|b| i64::try_from(b).ok())
        .ok_or_else(|| {
            SandboxError::InvalidConfig(format!("memory_mb {} too large", r.memory_mb))
        })?;
    let nano_cpus = i64::from(r.vcpu) * 1_000_000_000;

    let cwd = guest_str(&spec.default_cwd)?.to_owned();
    let user = effective_user(spec, ctx.host_user);
    let labels = base_labels(spec, ctx.owner, ROLE_SANDBOX);
    let stop_grace = i64::try_from(ctx.limits.stop_grace.as_secs()).unwrap_or(i64::MAX);

    Ok(ContainerCreateBody {
        hostname: Some(container_name(spec.id)),
        user: Some(user.to_string()),
        env: Some(env),
        // Entrypoint pinned to crabd so an image ENTRYPOINT can never wrap it.
        entrypoint: Some(vec![GUEST_CRABD.to_owned()]),
        cmd: Some(vec![
            "serve".into(),
            "--unix".into(),
            guest_socket(ctx.transport),
            "--default-cwd".into(),
            cwd.clone(),
            "--auth-token-file".into(),
            format!("{GUEST_RUN_DIR}/{TOKEN_NAME}"),
        ]),
        image: Some(ctx.image.to_owned()),
        working_dir: Some(cwd),
        labels: Some(labels),
        exposed_ports: (!port_keys.is_empty()).then(|| port_keys.clone()),
        network_disabled: Some(false),
        stop_timeout: Some(stop_grace),
        attach_stdin: Some(false),
        open_stdin: Some(false),
        tty: Some(false),
        host_config: Some(HostConfig {
            network_mode: Some(mode.to_owned()),
            port_bindings: (!port_bindings.is_empty()).then_some(port_bindings),
            mounts: Some(mounts),
            cap_drop: Some(vec!["ALL".into()]),
            security_opt: Some(vec!["no-new-privileges:true".into()]),
            privileged: Some(false),
            init: Some(true),
            ipc_mode: Some("private".into()),
            readonly_rootfs: Some(false),
            pids_limit: Some(ctx.limits.pids_limit),
            memory: Some(memory),
            memory_swap: Some(memory),
            nano_cpus: Some(nano_cpus),
            tmpfs: Some(HashMap::from([(
                "/tmp".to_owned(),
                format!(
                    "rw,nosuid,nodev,noexec,size={}m,mode=1777",
                    ctx.limits.tmp_size_mb
                ),
            )])),
            ulimits: Some(vec![ResourcesUlimits {
                name: Some("core".into()),
                soft: Some(0),
                hard: Some(0),
            }]),
            auto_remove: Some(false),
            publish_all_ports: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Definition of [`RELAY_NETWORK`].
#[must_use]
pub fn relay_network_spec() -> bollard::models::NetworkCreateRequest {
    bollard::models::NetworkCreateRequest {
        name: RELAY_NETWORK.to_owned(),
        driver: Some("bridge".into()),
        internal: Some(false),
        attachable: Some(false),
        enable_ipv6: Some(false),
        options: Some(
            RELAY_NETWORK_OPTIONS
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        ),
        labels: Some(HashMap::from([(
            LABEL_MANAGED.to_owned(),
            "true".to_owned(),
        )])),
        ..Default::default()
    }
}

/// Check that an existing [`RELAY_NETWORK`] is fit for the relays: a bridge carrying
/// [`RELAY_NETWORK_OPTIONS`]. A network of that name with inter-container traffic on would
/// silently expose every relay to its neighbours, so it is refused, never used.
pub fn check_relay_network(
    driver: Option<&str>,
    options: Option<&HashMap<String, String>>,
) -> std::result::Result<(), String> {
    if driver != Some("bridge") {
        return Err(format!(
            "driver is {}, not bridge",
            driver.unwrap_or("unknown")
        ));
    }
    for (k, v) in RELAY_NETWORK_OPTIONS {
        let got = options.and_then(|o| o.get(k)).map(String::as_str);
        if got != Some(v) {
            return Err(format!("option {k} is {}, not {v}", got.unwrap_or("unset")));
        }
    }
    Ok(())
}

/// The TCP transport's relay container: `crabd relay` from the socket volume to
/// [`RELAY_PORT`], published on host `127.0.0.1` (ephemeral port) only. It runs no user code
/// and does no auth itself (the sandbox's `crabd` authenticates every relayed connection).
/// Hardened like the sandbox, plus a read-only root filesystem and small fixed limits.
pub fn build_relay_spec(spec: &ResolvedSpec, ctx: &SpecContext<'_>) -> Result<ContainerCreateBody> {
    let user = effective_user(spec, ctx.host_user);
    let port_key = format!("{RELAY_PORT}/tcp");
    let stop_grace = i64::try_from(ctx.limits.stop_grace.as_secs()).unwrap_or(i64::MAX);
    Ok(ContainerCreateBody {
        user: Some(user.to_string()),
        env: Some(Vec::new()),
        entrypoint: Some(vec![GUEST_CRABD.to_owned()]),
        cmd: Some(vec![
            "relay".into(),
            "--tcp".into(),
            format!("0.0.0.0:{RELAY_PORT}"),
            "--unix".into(),
            guest_socket(DockerTransport::Tcp),
        ]),
        image: Some(ctx.image.to_owned()),
        working_dir: Some("/".into()),
        labels: Some(base_labels(spec, ctx.owner, ROLE_RELAY)),
        exposed_ports: Some(vec![port_key.clone()]),
        network_disabled: Some(false),
        stop_timeout: Some(stop_grace),
        attach_stdin: Some(false),
        open_stdin: Some(false),
        tty: Some(false),
        host_config: Some(HostConfig {
            network_mode: Some(RELAY_NETWORK.into()),
            port_bindings: Some(loopback_binding(&port_key)),
            mounts: Some(vec![
                bind(ctx.crabd_path, GUEST_CRABD, true)?,
                volume(&sock_volume_name(spec.id), GUEST_SOCK_DIR, true),
            ]),
            cap_drop: Some(vec!["ALL".into()]),
            security_opt: Some(vec!["no-new-privileges:true".into()]),
            privileged: Some(false),
            init: Some(true),
            ipc_mode: Some("private".into()),
            readonly_rootfs: Some(true),
            pids_limit: Some(RELAY_PIDS_LIMIT),
            memory: Some(RELAY_MEMORY),
            memory_swap: Some(RELAY_MEMORY),
            nano_cpus: Some(RELAY_NANO_CPUS),
            ulimits: Some(vec![ResourcesUlimits {
                name: Some("core".into()),
                soft: Some(0),
                hard: Some(0),
            }]),
            auto_remove: Some(false),
            publish_all_ports: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use sandcrab_core::{
        LifecycleTimings, MountMode, PortSpec, ResolvedMount, ResourceLimits, SecretSource,
        SecretSpec, Template, TemplateManifest, WorkspaceId,
    };

    use super::*;

    const MANIFEST: &str = r#"
[template]
name = "base"
version = "0.1.0"
description = "test"
image = "sandcrab/base:0.1.0"
"#;

    fn spec() -> ResolvedSpec {
        let manifest = TemplateManifest::from_toml_str(MANIFEST).unwrap();
        ResolvedSpec {
            id: SandboxId::new(),
            image: manifest.image().to_owned(),
            image_sha256: None,
            template: Template {
                manifest,
                dir: PathBuf::from("/templates/base"),
            },
            mounts: Vec::new(),
            default_cwd: PathBuf::from("/workspace"),
            env: BTreeMap::new(),
            network: NetworkConfig::disabled(),
            ports: Vec::new(),
            user: None,
            resources: ResourceLimits {
                vcpu: 2,
                memory_mb: 512,
                disk_mb: 2048,
            },
            ttl: None,
            secrets: Vec::new(),
            timings: LifecycleTimings::start(),
        }
    }

    fn mount(name: &str, host: &str, guest: &str, mode: MountMode) -> ResolvedMount {
        ResolvedMount {
            workspace_id: WorkspaceId::new(),
            workspace_name: name.into(),
            backing: WorkspaceBacking::HostDir(host.into()),
            host_path: host.into(),
            guest_path: guest.into(),
            mode,
            generation: 1,
            root_identity: None,
        }
    }

    fn build(spec: &ResolvedSpec) -> Result<ContainerCreateBody> {
        build_with(spec, DockerTransport::Unix, false)
    }

    fn build_with(
        spec: &ResolvedSpec,
        transport: DockerTransport,
        relay: bool,
    ) -> Result<ContainerCreateBody> {
        let limits = DockerLimits::default();
        let f = if relay {
            build_relay_spec
        } else {
            build_container_spec
        };
        f(
            spec,
            &SpecContext {
                crabd_path: Path::new("/host/crabd"),
                socket_dir: Path::new("/state/run/abc"),
                image: "sha256:feed",
                host_user: UserSpec::new(1234, 5678),
                limits: &limits,
                owner: &Owner {
                    uid: 1234,
                    pid: 42,
                    boot_id: "boot-a".into(),
                    state_dir: "0123456789abcdef".into(),
                },
                transport,
            },
        )
    }

    #[test]
    fn policy_mapping_matrix() {
        assert_eq!(network_mode(&NetworkConfig::disabled()).unwrap(), "none");
        assert_eq!(
            network_mode(&NetworkConfig::unrestricted()).unwrap(),
            "bridge"
        );
        let err = network_mode(&NetworkConfig::egress_restricted(["pypi.org"])).unwrap_err();
        assert!(matches!(err, SandboxError::PolicyUnenforceable(_)), "{err}");

        let mut s = spec();
        s.network = NetworkConfig::egress_restricted(["pypi.org"]);
        assert!(matches!(
            build(&s),
            Err(SandboxError::PolicyUnenforceable(_))
        ));
    }

    #[test]
    fn hardened_host_config() {
        let body = build(&spec()).unwrap();
        let hc = body.host_config.unwrap();
        assert_eq!(hc.network_mode.as_deref(), Some("none"));
        assert_eq!(hc.cap_drop, Some(vec!["ALL".to_owned()]));
        assert_eq!(
            hc.security_opt,
            Some(vec!["no-new-privileges:true".to_owned()])
        );
        assert_eq!(hc.privileged, Some(false));
        assert_eq!(hc.init, Some(true));
        assert_eq!(hc.ipc_mode.as_deref(), Some("private"));
        assert_eq!(hc.readonly_rootfs, Some(false));
        assert_eq!(hc.pids_limit, Some(1024));
        assert_eq!(hc.memory, Some(512 * 1024 * 1024));
        assert_eq!(hc.memory_swap, hc.memory);
        assert_eq!(hc.nano_cpus, Some(2_000_000_000));
        assert!(hc.cap_add.is_none());
        assert!(hc.port_bindings.is_none());
        assert_eq!(
            hc.tmpfs.unwrap()["/tmp"],
            "rw,nosuid,nodev,noexec,size=256m,mode=1777"
        );
        let ulimits = hc.ulimits.unwrap();
        assert_eq!(ulimits.len(), 1);
        assert_eq!(ulimits[0].name.as_deref(), Some("core"));
        assert_eq!((ulimits[0].soft, ulimits[0].hard), (Some(0), Some(0)));
    }

    #[test]
    fn cmd_user_env_labels() {
        let mut s = spec();
        s.env.insert("FOO".into(), "bar=baz".into());
        let body = build(&s).unwrap();
        assert_eq!(body.image.as_deref(), Some("sha256:feed"));
        assert_eq!(body.entrypoint, Some(vec![GUEST_CRABD.to_owned()]));
        assert_eq!(
            body.cmd.unwrap(),
            [
                "serve",
                "--unix",
                "/run/sandcrab/crabd.sock",
                "--default-cwd",
                "/workspace",
                "--auth-token-file",
                "/run/sandcrab/auth-token"
            ]
        );
        assert_eq!(body.working_dir.as_deref(), Some("/workspace"));
        assert_eq!(body.user.as_deref(), Some("1234:5678"));
        let env = body.env.unwrap();
        assert!(env.contains(&"FOO=bar=baz".to_owned()));
        assert!(env.contains(&"HOME=/home/sandcrab".to_owned()));
        let labels = body.labels.unwrap();
        assert_eq!(labels[LABEL_MANAGED], "true");
        assert_eq!(labels[LABEL_SANDBOX_ID], s.id.to_string());
        assert_eq!(labels[LABEL_TEMPLATE], "base");
        assert_eq!(labels[LABEL_BACKEND], "docker");
        assert_eq!(labels[LABEL_OWNER_UID], "1234");
        assert_eq!(labels[LABEL_OWNER_PID], "42");
        assert_eq!(labels[LABEL_OWNER_BOOT_ID], "boot-a");
        assert_eq!(labels[LABEL_STATE_DIR], "0123456789abcdef");
        assert_eq!(labels[LABEL_ROLE], ROLE_SANDBOX);
        assert_eq!(body.hostname.unwrap(), container_name(s.id));
        assert_eq!(body.stop_timeout, Some(2));

        // Overrides win.
        s.user = Some(UserSpec::new(1000, 1000));
        s.env.insert("HOME".into(), "/root".into());
        let body = build(&s).unwrap();
        assert_eq!(body.user.as_deref(), Some("1000:1000"));
        let env = body.env.unwrap();
        assert!(env.contains(&"HOME=/root".to_owned()));
        assert!(!env.contains(&"HOME=/home/sandcrab".to_owned()));

        s.env.insert("A=B".into(), "x".into());
        assert!(matches!(build(&s), Err(SandboxError::InvalidConfig(_))));
    }

    #[test]
    fn mounts_crabd_socket_and_workspaces() {
        let mut s = spec();
        s.mounts = vec![
            mount("proj", "/home/u/proj", "/workspace", MountMode::ReadWrite),
            mount("docs", "/home/u/docs", "/ref/docs", MountMode::ReadOnly),
        ];
        let mounts = build(&s).unwrap().host_config.unwrap().mounts.unwrap();
        let view: Vec<_> = mounts
            .iter()
            .map(|m| {
                assert_eq!(m.typ, Some(MountType::BIND));
                (
                    m.source.as_deref().unwrap(),
                    m.target.as_deref().unwrap(),
                    m.read_only.unwrap(),
                )
            })
            .collect();
        assert_eq!(
            view,
            [
                ("/host/crabd", "/opt/sandcrab/crabd", true),
                ("/state/run/abc", "/run/sandcrab", false),
                ("/home/u/proj", "/workspace", false),
                ("/home/u/docs", "/ref/docs", true),
            ]
        );

        s.mounts = vec![mount("x", "/h/x", "/run/sandcrab/x", MountMode::ReadWrite)];
        assert!(build(&s).is_err());
        s.mounts = vec![mount("x", "relative", "/x", MountMode::ReadWrite)];
        assert!(build(&s).is_err());
        let mut block = mount("v", "/h/v.img", "/data", MountMode::ReadWrite);
        block.backing = WorkspaceBacking::BlockVolume("/h/v.img".into());
        s.mounts = vec![block];
        assert!(matches!(build(&s), Err(SandboxError::InvalidConfig(_))));
    }

    #[test]
    fn ports_need_network_and_bind_loopback() {
        let mut s = spec();
        s.ports = vec![PortSpec { guest: 8080 }];
        let err = build(&s).unwrap_err().to_string();
        assert!(err.contains("unrestricted"), "{err}");

        s.network = NetworkConfig::unrestricted();
        let body = build(&s).unwrap();
        assert_eq!(body.exposed_ports, Some(vec!["8080/tcp".to_owned()]));
        let hc = body.host_config.unwrap();
        assert_eq!(hc.network_mode.as_deref(), Some("bridge"));
        let bindings = hc.port_bindings.unwrap()["8080/tcp"].clone().unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(bindings[0].host_port.as_deref(), Some(""));

        s.ports.push(PortSpec { guest: 8080 });
        assert!(build(&s).is_err());
        s.ports = vec![PortSpec { guest: 0 }];
        assert!(build(&s).is_err());
    }

    #[test]
    fn tcp_transport_keeps_sandbox_network_and_shares_a_socket_volume() {
        for (policy, mode) in [
            (NetworkConfig::disabled(), "none"),
            (NetworkConfig::unrestricted(), "bridge"),
        ] {
            let mut s = spec();
            s.network = policy;
            let body = build_with(&s, DockerTransport::Tcp, false).unwrap();
            let cmd = body.cmd.unwrap();
            assert_eq!(cmd[2], "/opt/sandcrab/sock/crabd.sock");
            assert!(cmd.ends_with(&[
                "--auth-token-file".to_owned(),
                "/run/sandcrab/auth-token".to_owned()
            ]));
            assert!(
                body.exposed_ports.is_none(),
                "crabd itself is never exposed"
            );
            let hc = body.host_config.unwrap();
            assert_eq!(hc.network_mode.as_deref(), Some(mode), "policy unchanged");
            assert!(hc.port_bindings.is_none());
            let vol = hc
                .mounts
                .unwrap()
                .into_iter()
                .find(|m| m.typ == Some(MountType::VOLUME))
                .unwrap();
            assert_eq!(vol.source, Some(sock_volume_name(s.id)));
            assert_eq!(vol.target.as_deref(), Some(GUEST_SOCK_DIR));
            assert_eq!(vol.read_only, Some(false));
        }
        // Unix transport: no volume.
        let hc = build(&spec()).unwrap().host_config.unwrap();
        assert!(
            hc.mounts
                .unwrap()
                .iter()
                .all(|m| m.typ == Some(MountType::BIND))
        );
    }

    #[test]
    fn relay_network_has_icc_off_and_is_checked() {
        let n = relay_network_spec();
        assert_eq!(n.name, RELAY_NETWORK);
        assert_eq!(n.driver.as_deref(), Some("bridge"));
        assert_eq!(
            n.internal,
            Some(false),
            "internal networks cannot publish ports"
        );
        let opts = n.options.unwrap();
        assert_eq!(opts["com.docker.network.bridge.enable_icc"], "false");
        check_relay_network(Some("bridge"), Some(&opts)).unwrap();
        let mut icc_on = opts.clone();
        icc_on.insert("com.docker.network.bridge.enable_icc".into(), "true".into());
        let e = check_relay_network(Some("bridge"), Some(&icc_on)).unwrap_err();
        assert!(e.contains("enable_icc"), "{e}");
        assert!(check_relay_network(Some("bridge"), None).is_err());
        assert!(check_relay_network(Some("overlay"), Some(&opts)).is_err());
    }

    #[test]
    fn relay_is_hardened_and_published_on_loopback_only() {
        let s = spec();
        let body = build_with(&s, DockerTransport::Tcp, true).unwrap();
        assert_eq!(body.entrypoint, Some(vec![GUEST_CRABD.to_owned()]));
        assert_eq!(
            body.cmd.unwrap(),
            [
                "relay",
                "--tcp",
                "0.0.0.0:7070",
                "--unix",
                "/opt/sandcrab/sock/crabd.sock"
            ]
        );
        assert_eq!(body.user.as_deref(), Some("1234:5678"));
        assert_eq!(body.env, Some(Vec::new()));
        assert_eq!(body.exposed_ports, Some(vec!["7070/tcp".to_owned()]));
        let labels = body.labels.unwrap();
        assert_eq!(labels[LABEL_ROLE], ROLE_RELAY);
        assert_eq!(labels[LABEL_SANDBOX_ID], s.id.to_string());
        assert_eq!(labels[LABEL_MANAGED], "true");
        assert_eq!(labels[LABEL_OWNER_PID], "42");
        let hc = body.host_config.unwrap();
        let b = hc.port_bindings.unwrap()["7070/tcp"].clone().unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].host_ip.as_deref(), Some("127.0.0.1"), "never 0.0.0.0");
        assert_eq!(b[0].host_port.as_deref(), Some(""), "ephemeral host port");
        assert_eq!(hc.publish_all_ports, Some(false));
        assert_eq!(
            hc.network_mode.as_deref(),
            Some(RELAY_NETWORK),
            "never the default bridge"
        );
        assert_eq!(hc.cap_drop, Some(vec!["ALL".to_owned()]));
        assert!(hc.cap_add.is_none());
        assert_eq!(
            hc.security_opt,
            Some(vec!["no-new-privileges:true".to_owned()])
        );
        assert_eq!(hc.privileged, Some(false));
        assert_eq!(hc.readonly_rootfs, Some(true));
        assert_eq!(hc.pids_limit, Some(64));
        assert_eq!(hc.memory, Some(64 * 1024 * 1024));
        assert_eq!(hc.memory_swap, hc.memory);
        let mounts: Vec<_> = hc
            .mounts
            .unwrap()
            .into_iter()
            .map(|m| (m.source.unwrap(), m.target.unwrap(), m.read_only.unwrap()))
            .collect();
        assert_eq!(
            mounts,
            [
                ("/host/crabd".to_owned(), GUEST_CRABD.to_owned(), true),
                (sock_volume_name(s.id), GUEST_SOCK_DIR.to_owned(), true),
            ],
            "no workspaces, no run dir (token) in the relay"
        );
    }

    #[test]
    fn sock_volume_is_a_private_tmpfs() {
        let s = spec();
        let owner = Owner {
            uid: 1234,
            pid: 42,
            boot_id: "b".into(),
            state_dir: "k".into(),
        };
        let v = sock_volume_spec(&s, UserSpec::new(1234, 5678), &owner);
        assert_eq!(v.name, Some(sock_volume_name(s.id)));
        assert_eq!(v.driver.as_deref(), Some("local"));
        let o = v.driver_opts.unwrap();
        assert_eq!(o["type"], "tmpfs");
        assert_eq!(
            o["o"],
            "size=1m,mode=0700,uid=1234,gid=5678,nosuid,nodev,noexec"
        );
        let labels = v.labels.unwrap();
        assert_eq!(labels[LABEL_SANDBOX_ID], s.id.to_string());
        assert_eq!(labels[LABEL_OWNER_UID], "1234");
        assert!(sock_volume_name(s.id).starts_with(&container_name(s.id)));
        assert_ne!(relay_name(s.id), container_name(s.id));
    }

    #[test]
    fn secrets_rejected() {
        let mut s = spec();
        s.secrets.push(SecretSpec {
            name: "K".into(),
            source: SecretSource::HostEnv { var: "K".into() },
        });
        assert!(matches!(
            build(&s),
            Err(SandboxError::PolicyUnenforceable(_))
        ));
    }

    #[test]
    fn names_are_short_and_stable() {
        let id = SandboxId::new();
        let n = container_name(id);
        assert_eq!(n.len(), "sandcrab-".len() + 12);
        assert_eq!(n, container_name(id));
    }
}
