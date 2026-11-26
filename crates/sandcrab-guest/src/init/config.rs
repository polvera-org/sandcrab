//! Validation of [`InitConfig`] and the workspace mount plan derived from it.
//!
//! Everything here is pure: the host already validated the same config, but the guest
//! re-checks it (defence in depth) before acting on it as root.

use std::{
    collections::BTreeSet,
    io,
    path::{Component, Path, PathBuf},
};

use sandcrab_protocol::init::{
    CONTROL_PORT, GuestMount, InitConfig, MAX_DRIVE_ID_LEN, MAX_EXT4_LABEL_LEN,
    MAX_VIRTIOFS_TAG_LEN, MountSource, ROOT_DRIVE_ID, SCRATCH_DRIVE_ID,
};

/// Guest paths that may never be (or contain) a workspace mount point.
pub const FORBIDDEN_MOUNT_PREFIXES: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/mnt/scratch",
    "/opt/sandcrab",
];

const MAX_HOSTNAME_LEN: usize = 64;

pub(crate) fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

/// Normalise an absolute guest path: UTF-8, no NUL, no `..`; `.` and repeated `/` dropped.
pub fn normalize_abs(path: &Path) -> io::Result<PathBuf> {
    let bad = |why: &str| invalid(format!("guest path `{}` {why}", path.display()));
    let s = path.to_str().ok_or_else(|| bad("is not valid UTF-8"))?;
    if s.contains('\0') {
        return Err(bad("contains a NUL byte"));
    }
    if !path.is_absolute() {
        return Err(bad("must be absolute"));
    }
    let mut out = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(p) => out.push(p),
            Component::ParentDir => return Err(bad("must not contain `..`")),
            Component::Prefix(_) => return Err(bad("must be a Unix path")),
        }
    }
    Ok(out)
}

/// Validate a workspace mount point: normalised, not `/`, not under a forbidden prefix.
pub fn validate_mount_point(path: &Path) -> io::Result<PathBuf> {
    let p = normalize_abs(path)?;
    if p == Path::new("/") {
        return Err(invalid("a workspace cannot be mounted at `/`"));
    }
    for prefix in FORBIDDEN_MOUNT_PREFIXES {
        if p.starts_with(prefix) {
            return Err(invalid(format!(
                "guest path `{}` is under reserved `{prefix}`",
                p.display()
            )));
        }
    }
    Ok(p)
}

/// `drive_id`: `[A-Za-z0-9_]`, 1..=20 bytes (virtio-blk serial limit), not a reserved drive.
pub fn validate_drive_id(id: &str) -> io::Result<()> {
    if id.is_empty() || id.len() > MAX_DRIVE_ID_LEN {
        return Err(invalid(format!(
            "drive_id `{id}` must be 1..={MAX_DRIVE_ID_LEN} bytes"
        )));
    }
    if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(invalid(format!("drive_id `{id}` must match [A-Za-z0-9_]+")));
    }
    if id == ROOT_DRIVE_ID || id == SCRATCH_DRIVE_ID {
        return Err(invalid(format!("drive_id `{id}` is reserved")));
    }
    Ok(())
}

fn validate_hostname(name: &str) -> io::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= MAX_HOSTNAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && !name.starts_with(['-', '.']);
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "hostname `{name}` must be 1..={MAX_HOSTNAME_LEN} chars of [A-Za-z0-9.-]"
        )))
    }
}

fn validate_env(cfg: &InitConfig) -> io::Result<()> {
    for (k, v) in &cfg.env {
        if k.is_empty() || k.contains(['=', '\0']) {
            return Err(invalid(format!("invalid env var name `{k}`")));
        }
        if v.contains('\0') {
            return Err(invalid(format!("env var `{k}` contains a NUL byte")));
        }
    }
    Ok(())
}

/// virtio-fs `tag`: `[A-Za-z0-9_.:-]`, 1..=36 bytes (the virtio-fs config field size).
pub fn validate_virtiofs_tag(tag: &str) -> io::Result<()> {
    if tag.is_empty() || tag.len() > MAX_VIRTIOFS_TAG_LEN {
        return Err(invalid(format!(
            "virtio-fs tag `{tag}` must be 1..={MAX_VIRTIOFS_TAG_LEN} bytes"
        )));
    }
    if !tag
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
    {
        return Err(invalid(format!(
            "virtio-fs tag `{tag}` must match [A-Za-z0-9_.:-]+"
        )));
    }
    Ok(())
}

/// Where a planned workspace mount comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepSource {
    /// virtio-blk drive holding an ext4 filesystem.
    Block {
        drive_id: String,
        /// ext4 label used when no device serial matches.
        label: String,
    },
    /// virtio-fs share (libkrun), mounted by tag.
    VirtioFs { tag: String },
}

/// One workspace mount, validated, in mount order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountStep {
    pub source: StepSource,
    /// Normalised absolute mount point.
    pub target: PathBuf,
    /// `MS_RDONLY`. For virtio-fs the host VMM enforces it too; the guest flag is defence
    /// in depth.
    pub read_only: bool,
}

impl MountStep {
    /// The block `drive_id`, or `None` for a virtio-fs share.
    #[must_use]
    pub fn drive_id(&self) -> Option<&str> {
        match &self.source {
            StepSource::Block { drive_id, .. } => Some(drive_id),
            StepSource::VirtioFs { .. } => None,
        }
    }

    /// `mount(2)` filesystem type.
    #[must_use]
    pub fn fstype(&self) -> &'static str {
        match self.source {
            StepSource::Block { .. } => "ext4",
            StepSource::VirtioFs { .. } => "virtiofs",
        }
    }

    /// Filesystem-specific mount data: `noload` skips journal replay on a read-only ext4
    /// drive (which would otherwise write to it). virtio-fs takes none.
    #[must_use]
    pub fn data(&self) -> &'static str {
        match self.source {
            StepSource::Block { .. } if self.read_only => "noload",
            _ => "",
        }
    }

    /// Whether to add `MS_NOATIME` (block drives; virtio-fs leaves atime to the host).
    #[must_use]
    pub fn noatime(&self) -> bool {
        matches!(self.source, StepSource::Block { .. })
    }
}

/// Validate `mounts` and order them parents-first (stable for equal depth).
pub fn plan_mounts(mounts: &[GuestMount]) -> io::Result<Vec<MountStep>> {
    let mut ids = BTreeSet::new();
    let mut tags = BTreeSet::new();
    let mut targets = BTreeSet::new();
    let mut steps = Vec::with_capacity(mounts.len());
    for m in mounts {
        let source = match &m.source {
            MountSource::Block { drive_id, label } => {
                validate_drive_id(drive_id)?;
                if !ids.insert(drive_id.clone()) {
                    return Err(invalid(format!("duplicate drive_id `{drive_id}`")));
                }
                let label = label.clone().unwrap_or_else(|| drive_id.clone());
                if label.is_empty() || label.len() > MAX_EXT4_LABEL_LEN || label.contains('\0') {
                    return Err(invalid(format!(
                        "ext4 label `{label}` must be 1..={MAX_EXT4_LABEL_LEN} bytes"
                    )));
                }
                StepSource::Block {
                    drive_id: drive_id.clone(),
                    label,
                }
            }
            MountSource::VirtioFs { tag } => {
                validate_virtiofs_tag(tag)?;
                if !tags.insert(tag.clone()) {
                    return Err(invalid(format!("duplicate virtio-fs tag `{tag}`")));
                }
                StepSource::VirtioFs { tag: tag.clone() }
            }
        };
        let target = validate_mount_point(&m.guest_path)?;
        if !targets.insert(target.clone()) {
            return Err(invalid(format!(
                "two workspaces mounted at `{}`",
                target.display()
            )));
        }
        steps.push(MountStep {
            source,
            target,
            read_only: m.read_only,
        });
    }
    steps.sort_by_key(|s| s.target.components().count());
    Ok(steps)
}

/// A validated config plus its mount plan.
#[derive(Debug, Clone)]
pub struct ValidatedConfig {
    pub config: InitConfig,
    pub default_cwd: PathBuf,
    pub mounts: Vec<MountStep>,
}

/// Validate everything `crabd init` acts on as root.
pub fn validate_config(cfg: InitConfig) -> io::Result<ValidatedConfig> {
    validate_hostname(&cfg.hostname)?;
    validate_env(&cfg)?;
    // vsock ports below 1024 need CAP_NET_BIND_SERVICE and `crabd serve` runs unprivileged.
    // (This also keeps it off the control port.)
    if cfg.tools_port < 1024 {
        return Err(invalid(format!(
            "tools_port {} must be >= 1024 (the control port is {CONTROL_PORT})",
            cfg.tools_port
        )));
    }
    let default_cwd = normalize_abs(&cfg.default_cwd)?;
    if let Some(home) = &cfg.user.home {
        normalize_abs(home)?;
    }
    if let Some(name) = &cfg.user.name
        && (name.is_empty() || name.contains(['\0', '/', ':']))
    {
        return Err(invalid(format!("invalid user name `{name}`")));
    }
    let mounts = plan_mounts(&cfg.mounts)?;
    Ok(ValidatedConfig {
        config: cfg,
        default_cwd,
        mounts,
    })
}

/// `/etc/resolv.conf` contents for `dns`.
#[must_use]
pub fn resolv_conf(dns: &[std::net::IpAddr]) -> String {
    let mut s = String::from("# written by crabd init\n");
    for ip in dns {
        s.push_str(&format!("nameserver {ip}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandcrab_protocol::init::InitUser;

    fn gm(id: &str, path: &str, ro: bool) -> GuestMount {
        GuestMount::block(id, None, path, ro)
    }

    fn cfg(mounts: Vec<GuestMount>) -> InitConfig {
        InitConfig {
            sandbox_id: "sb".into(),
            hostname: "sandcrab".into(),
            user: InitUser {
                uid: 1000,
                gid: 1000,
                home: None,
                name: Some("agent".into()),
            },
            env: [("A".to_owned(), "b".to_owned())].into(),
            default_cwd: "/workspace/./".into(),
            mounts,
            network: false,
            dns: vec![],
            tools_port: 1024,
            host_time_unix_ms: None,
            tmp_exec: false,
            tmp_size_mb: None,
            pids_max: None,
            nofile: None,
            forward_ports: Vec::new(),
        }
    }

    #[test]
    fn plan_orders_parents_first_and_sets_options() {
        let plan = plan_mounts(&[
            gm("ws1", "/workspace/data//", true),
            gm("ws0", "/workspace", false),
        ])
        .unwrap();
        assert_eq!(plan[0].target, Path::new("/workspace"));
        assert_eq!(plan[0].data(), "");
        assert_eq!(plan[1].target, Path::new("/workspace/data"));
        assert!(plan[1].read_only);
        assert_eq!(plan[1].data(), "noload");
        assert_eq!(
            plan[1].source,
            StepSource::Block {
                drive_id: "ws1".into(),
                label: "ws1".into()
            }
        );
        assert_eq!(plan[1].drive_id(), Some("ws1"));
        assert_eq!(plan[1].fstype(), "ext4");
        assert!(plan[1].noatime());
    }

    #[test]
    fn plan_virtio_fs_mounts() {
        let plan = plan_mounts(&[
            GuestMount::virtio_fs("sandcrab-ro", "/workspace/ref/", true),
            GuestMount::virtio_fs("ws0", "/workspace", false),
            gm("vol0", "/data", false),
        ])
        .unwrap();
        assert_eq!(plan[0].target, Path::new("/workspace"));
        assert_eq!(plan[0].source, StepSource::VirtioFs { tag: "ws0".into() });
        assert_eq!(plan[0].fstype(), "virtiofs");
        assert_eq!(plan[0].data(), "");
        assert!(!plan[0].read_only);
        assert!(!plan[0].noatime());
        assert_eq!(plan[0].drive_id(), None);
        // Mixed with a block volume; equal depth keeps input order.
        assert_eq!(plan[1].target, Path::new("/data"));
        assert_eq!(plan[1].fstype(), "ext4");
        assert_eq!(plan[2].target, Path::new("/workspace/ref"));
        assert!(plan[2].read_only);
        // Read-only virtio-fs gets MS_RDONLY but no ext4 `noload`.
        assert_eq!(plan[2].data(), "");
        assert_eq!(
            plan[2].source,
            StepSource::VirtioFs {
                tag: "sandcrab-ro".into()
            }
        );
    }

    #[test]
    fn plan_rejects_bad_virtio_fs_mounts() {
        let vf = |tag: &str, path: &str| GuestMount::virtio_fs(tag, path, false);
        for (m, why) in [
            (vec![vf("", "/w")], "1..=36"),
            (vec![vf(&"t".repeat(37), "/w")], "1..=36"),
            (vec![vf("ws 0", "/w")], "[A-Za-z0-9_.:-]"),
            (vec![vf("ws0,ro", "/w")], "[A-Za-z0-9_.:-]"),
            (vec![vf("ws\0x", "/w")], "[A-Za-z0-9_.:-]"),
            (
                vec![vf("ws0", "/w"), vf("ws0", "/v")],
                "duplicate virtio-fs tag",
            ),
            (vec![vf("ws0", "/w"), vf("ws1", "/w/./")], "two workspaces"),
            (
                vec![vf("ws0", "/w"), gm("ws1", "/w", false)],
                "two workspaces",
            ),
            (vec![vf("ws0", "/dev/x")], "reserved"),
            (vec![vf("ws0", "/a/../b")], ".."),
            (vec![vf("ws0", "/")], "`/`"),
            (vec![vf("ws0", "w")], "absolute"),
        ] {
            let err = plan_mounts(&m).unwrap_err().to_string();
            assert!(err.contains(why), "{m:?}: {err}");
        }
        // A tag and a drive_id may share a name: they are different namespaces.
        let ok = [vf("ws0", "/w"), gm("ws0", "/v", false)];
        assert_eq!(plan_mounts(&ok).unwrap().len(), 2);
        assert_eq!(plan_mounts(&[vf(&"t".repeat(36), "/w")]).unwrap().len(), 1);
    }

    #[test]
    fn plan_rejects_bad_mounts() {
        for (m, why) in [
            (vec![gm("ws0", "relative", false)], "absolute"),
            (vec![gm("ws0", "/a/../etc", false)], ".."),
            (vec![gm("ws0", "/", false)], "`/`"),
            (vec![gm("ws0", "/proc/x", false)], "reserved"),
            (vec![gm("ws0", "/mnt/scratch", false)], "reserved"),
            (vec![gm("ws0", "/run/user", false)], "reserved"),
            (vec![gm("rootfs", "/w", false)], "reserved"),
            (vec![gm("scratch", "/w", false)], "reserved"),
            (vec![gm("ws-0", "/w", false)], "[A-Za-z0-9_]"),
            (vec![gm("a_very_long_drive_id_x", "/w", false)], "1..=20"),
            (
                vec![gm("ws0", "/w", false), gm("ws0", "/v", false)],
                "duplicate",
            ),
            (
                vec![gm("ws0", "/w", false), gm("ws1", "/w/", false)],
                "two workspaces",
            ),
        ] {
            let err = plan_mounts(&m).unwrap_err().to_string();
            assert!(err.contains(why), "{m:?}: {err}");
        }
        let long_label = GuestMount::block("ws0", Some("x".repeat(17)), "/w", false);
        assert!(plan_mounts(&[long_label]).is_err());
    }

    #[test]
    fn config_validation() {
        let v = validate_config(cfg(vec![gm("ws0", "/workspace", false)])).unwrap();
        assert_eq!(v.default_cwd, Path::new("/workspace"));

        let mut c = cfg(vec![]);
        c.hostname = "bad host".into();
        assert!(validate_config(c).is_err());
        let mut c = cfg(vec![]);
        c.env.insert("A=B".into(), "x".into());
        assert!(validate_config(c).is_err());
        let mut c = cfg(vec![]);
        c.tools_port = CONTROL_PORT;
        assert!(validate_config(c).is_err());
        let mut c = cfg(vec![]);
        c.default_cwd = "workspace".into();
        assert!(validate_config(c).is_err());
        let mut c = cfg(vec![]);
        c.user.home = Some("../x".into());
        assert!(validate_config(c).is_err());
    }

    #[test]
    fn resolv_conf_lists_nameservers() {
        let s = resolv_conf(&["1.1.1.1".parse().unwrap(), "::1".parse().unwrap()]);
        assert!(s.contains("nameserver 1.1.1.1\nnameserver ::1\n"));
    }
}
