//! Pure translation of a [`ResolvedSpec`] into a hardened Docker container definition.
//!
//! Nothing here talks to Docker, so the whole hardening / mount / policy matrix is
//! unit-tested without a daemon.
//!
//! Hardening applied to every container:
//! - `cap_drop: ALL`, `no-new-privileges`, never privileged, private IPC namespace;
//! - `init: true` (docker-init as PID 1 reaps zombies and forwards SIGTERM to `crabd`);
//! - `pids_limit`, `memory` with `memory_swap == memory` (no swap), `nano_cpus`;
//! - size-limited `nosuid,nodev` tmpfs at `/tmp`;
//! - runs as the **host** uid:gid unless the spec overrides it, so files written to
//!   bind-mounted workspaces stay editable on the host;
//! - network `none` unless the policy is `unrestricted`; published ports bind to
//!   `127.0.0.1` only.
//!
//! The root filesystem stays writable (`readonly_rootfs: false`): templates expect a
//! writable `HOME` (`/home/sandcrab`, 1777) and package caches; the container layer is
//! discarded on destroy. Template `disk_mb` is **not enforced** (overlay2 needs an
//! xfs+pquota backing fs for `storage_opt.size`).

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use bollard::models::{
    ContainerCreateBody, HostConfig, Mount, MountBindOptions, MountBindOptionsPropagationEnum,
    MountType, PortBinding,
};
use sandcrab_core::{
    NetworkConfig, NetworkPolicy, ResolvedSpec, Result, SandboxError, SandboxId, UserSpec,
    WorkspaceBacking, validate_mount_target,
};

use crate::DockerLimits;

/// Where the host `crabd` binary is bind-mounted (read-only) inside the container.
pub const GUEST_CRABD: &str = "/opt/sandcrab/crabd";
/// Guest runtime directory, backed by the per-sandbox host socket directory.
pub const GUEST_RUN_DIR: &str = "/run/sandcrab";
/// File name of the `crabd` Unix socket inside the run directory.
pub const SOCKET_NAME: &str = "crabd.sock";
/// Default `HOME` (the templates' world-writable home).
pub const DEFAULT_HOME: &str = "/home/sandcrab";

pub const LABEL_MANAGED: &str = "sandcrab.managed";
pub const LABEL_SANDBOX_ID: &str = "sandcrab.sandbox_id";
pub const LABEL_TEMPLATE: &str = "sandcrab.template";
pub const LABEL_BACKEND: &str = "sandcrab.backend";
/// Value of [`LABEL_BACKEND`].
pub const BACKEND_LABEL_VALUE: &str = "docker-dev";

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
}

/// Docker `network_mode` for a policy. `egress_restricted` cannot be enforced by plain
/// Docker networking and is refused, never downgraded.
pub fn network_mode(network: &NetworkConfig) -> Result<&'static str> {
    match network.policy {
        NetworkPolicy::Disabled => Ok("none"),
        NetworkPolicy::Unrestricted => Ok("bridge"),
        NetworkPolicy::EgressRestricted => Err(SandboxError::PolicyUnenforceable(
            "docker-dev cannot enforce network policy `egress_restricted` (no domain \
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
    let port_bindings: HashMap<String, Option<Vec<PortBinding>>> = port_keys
        .iter()
        .map(|k| {
            (
                k.clone(),
                Some(vec![PortBinding {
                    host_ip: Some("127.0.0.1".into()),
                    host_port: Some(String::new()),
                }]),
            )
        })
        .collect();

    // Mounts: crabd (ro), socket dir (rw), then workspaces (ro honored by the kernel).
    let mut mounts = vec![
        bind(ctx.crabd_path, GUEST_CRABD, true)?,
        bind(ctx.socket_dir, GUEST_RUN_DIR, false)?,
    ];
    for m in &spec.mounts {
        if let WorkspaceBacking::BlockVolume(_) = m.backing {
            return Err(SandboxError::InvalidConfig(format!(
                "workspace `{}` is a block volume; docker-dev only mounts directories",
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
    let labels = HashMap::from([
        (LABEL_MANAGED.to_owned(), "true".to_owned()),
        (LABEL_SANDBOX_ID.to_owned(), spec.id.to_string()),
        (LABEL_TEMPLATE.to_owned(), spec.template.name().to_owned()),
        (LABEL_BACKEND.to_owned(), BACKEND_LABEL_VALUE.to_owned()),
    ]);
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
            format!("{GUEST_RUN_DIR}/{SOCKET_NAME}"),
            "--default-cwd".into(),
            cwd.clone(),
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
                format!("rw,nosuid,nodev,size={}m,mode=1777", ctx.limits.tmp_size_mb),
            )])),
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
        }
    }

    fn build(spec: &ResolvedSpec) -> Result<ContainerCreateBody> {
        let limits = DockerLimits::default();
        build_container_spec(
            spec,
            &SpecContext {
                crabd_path: Path::new("/host/crabd"),
                socket_dir: Path::new("/state/run/abc"),
                image: "sha256:feed",
                host_user: UserSpec::new(1234, 5678),
                limits: &limits,
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
            "rw,nosuid,nodev,size=256m,mode=1777"
        );
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
                "/workspace"
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
        assert_eq!(labels[LABEL_BACKEND], "docker-dev");
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
