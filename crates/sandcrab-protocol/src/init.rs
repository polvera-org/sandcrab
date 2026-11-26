//! `init.*` control contract between a microVM backend (host) and `crabd init` (guest PID 1).
//!
//! `crabd init` serves these methods on the vsock **control port** ([`CONTROL_PORT`]) only.
//! They are never served on the tools port and never exposed to agents. Framing and the
//! JSON-RPC envelope are the same as for tool calls.
//!
//! | Method | Params | Result |
//! |---|---|---|
//! | [`METHOD_CONFIGURE`] | [`InitConfig`] | [`InitConfigureResult`] (once per boot) |
//! | [`METHOD_EXPORT`] | [`InitExportParams`] | [`InitExportAck`], then **raw tar bytes until EOF** ([`archive`]) |
//! | [`METHOD_SHUTDOWN`] | [`InitShutdownParams`] | [`InitShutdownResult`], then the guest reboots and the VMM exits |
//!
//! `ping` is also answered on the control port (readiness probe before `init.configure`).
//!
//! After an `init.export` ack the connection carries no more frames: the host reads the tar
//! stream to EOF and opens a new control connection for any further request.

use std::{collections::BTreeMap, net::IpAddr, path::PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// vsock port of the `init.*` control channel.
pub const CONTROL_PORT: u32 = 1;
/// Default vsock port of the tools channel (`crabd serve --vsock`).
pub const DEFAULT_TOOLS_PORT: u32 = 1024;

pub const METHOD_CONFIGURE: &str = "init.configure";
pub const METHOD_EXPORT: &str = "init.export";
pub const METHOD_SHUTDOWN: &str = "init.shutdown";

/// Firecracker `drive_id` of the read-only root filesystem.
pub const ROOT_DRIVE_ID: &str = "rootfs";
/// Firecracker `drive_id` (and ext4 label) of the per-sandbox scratch drive (overlay upper).
pub const SCRATCH_DRIVE_ID: &str = "scratch";
/// The guest sees a drive's `drive_id` through the virtio-blk serial, which is at most 20
/// bytes. Longer ids would be truncated, so they are rejected.
pub const MAX_DRIVE_ID_LEN: usize = 20;
/// ext4 volume labels are at most 16 bytes.
pub const MAX_EXT4_LABEL_LEN: usize = 16;
/// Default grace period of `init.shutdown`.
pub const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 500;

/// Max tasks (processes + threads) of a sandbox: the fork-bomb cap. The Docker backend's
/// default `pids_limit`; `crabd init` sets it as `pids.max` of the guest cgroup holding
/// `crabd serve` and every tool process.
pub const SANDBOX_PIDS_MAX: u32 = 1024;
/// Size of the `nosuid,nodev,noexec` tmpfs at `/tmp`, in MiB. The Docker backend's default
/// `tmp_size_mb`; `crabd init` mounts the same tmpfs in microVM guests.
pub const SANDBOX_TMP_SIZE_MB: u64 = 256;

/// One guest kernel sysctl that `crabd init` hardens before any sandbox code runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardeningSysctl {
    /// Dotted name (`kernel.kptr_restrict`), the file under `/proc/sys` with `.` → `/`.
    pub key: &'static str,
    /// Accepted hardened values, most preferred first: init writes them in order until one
    /// sticks. A value outside this list read back from a guest is a hardening failure.
    pub values: &'static [&'static str],
}

/// The guest sysctls `crabd init` sets on the microVM backends (Docker cannot: they are not
/// namespaced, so they stay host-level there; its seccomp profile covers the same syscalls).
/// Each is set only if the kernel has it; one that is missing or refused is logged, never
/// fatal. `/proc/sys` is then made read-only, so not even a uid-0 sandbox user can undo them.
pub const HARDENING_SYSCTLS: &[HardeningSysctl] = &[
    // No unprivileged user namespaces (the gate to most "unprivileged" kernel attack
    // surface). The first is a Debian/Ubuntu patch; the second is mainline.
    HardeningSysctl {
        key: "kernel.unprivileged_userns_clone",
        values: &["0"],
    },
    HardeningSysctl {
        key: "user.max_user_namespaces",
        values: &["0"],
    },
    // 1 is sticky (cannot be reset until reboot); 2 is the kernel's "off, but resettable".
    HardeningSysctl {
        key: "kernel.unprivileged_bpf_disabled",
        values: &["1", "2"],
    },
    HardeningSysctl {
        key: "vm.unprivileged_userfaultfd",
        values: &["0"],
    },
    // 2: io_uring_setup fails for everyone; 1: only for unprivileged processes.
    HardeningSysctl {
        key: "kernel.io_uring_disabled",
        values: &["2", "1"],
    },
    // 3 (Debian patch: no perf_event_open for unprivileged users); mainline treats >= 2
    // as 2 (no kernel profiling).
    HardeningSysctl {
        key: "kernel.perf_event_paranoid",
        values: &["3", "2"],
    },
    HardeningSysctl {
        key: "kernel.kptr_restrict",
        values: &["2"],
    },
    HardeningSysctl {
        key: "kernel.dmesg_restrict",
        values: &["1"],
    },
    HardeningSysctl {
        key: "fs.protected_symlinks",
        values: &["1"],
    },
    HardeningSysctl {
        key: "fs.protected_hardlinks",
        values: &["1"],
    },
    HardeningSysctl {
        key: "fs.protected_fifos",
        values: &["2", "1"],
    },
    HardeningSysctl {
        key: "fs.protected_regular",
        values: &["2", "1"],
    },
    HardeningSysctl {
        key: "fs.suid_dumpable",
        values: &["0"],
    },
];

/// `/proc` paths `crabd init` bind-mounts read-only over themselves: Docker's default
/// `ReadonlyPaths`. Missing ones are skipped.
pub const PROC_READONLY_PATHS: &[&str] = &[
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

/// `init.configure` params: everything the guest needs that is not baked into the rootfs.
///
/// Delivered over vsock (not the kernel cmdline or MMDS): `env` may carry secrets, and the
/// cmdline is world-readable through `/proc/cmdline`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitConfig {
    /// Host-side sandbox id (logging / correlation only).
    pub sandbox_id: String,
    /// Guest hostname (`[A-Za-z0-9.-]`, at most 64 bytes).
    pub hostname: String,
    /// User `crabd serve` (and so every tool process) runs as.
    pub user: InitUser,
    /// Environment of `crabd serve` and the tool processes it starts.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Absolute default working directory for tools.
    pub default_cwd: PathBuf,
    /// Workspaces to mount (block drives or virtio-fs shares), in any order (the guest mounts
    /// parents first).
    #[serde(default)]
    pub mounts: Vec<GuestMount>,
    /// Whether the VM has a NIC (`eth0`, configured by the kernel `ip=` parameter).
    #[serde(default)]
    pub network: bool,
    /// Nameservers written to `/etc/resolv.conf` (only when `network` is true).
    #[serde(default)]
    pub dns: Vec<IpAddr>,
    /// vsock port for `crabd serve`.
    #[serde(default = "default_tools_port")]
    pub tools_port: u32,
    /// Host wall clock when the request was built, in ms since the Unix epoch. The guest sets
    /// `CLOCK_REALTIME` from it (a Firecracker guest has no RTC to read the time from).
    #[serde(default)]
    pub host_time_unix_ms: Option<u64>,
    /// Drop `noexec` from the `/tmp` tmpfs (`nosuid,nodev` stay).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tmp_exec: bool,
    /// Size of the `/tmp` tmpfs in MiB (counts against the sandbox's memory). Default
    /// [`SANDBOX_TMP_SIZE_MB`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmp_size_mb: Option<u64>,
    /// `pids.max` of the sandbox cgroup. Default [`SANDBOX_PIDS_MAX`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids_max: Option<u32>,
    /// `RLIMIT_NOFILE` of `crabd serve` (and so of every tool process). Default: inherited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nofile: Option<u64>,
    /// Loopback ports `crabd serve` listens on for forwards (`--forward-port`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forward_ports: Vec<u16>,
}

fn default_tools_port() -> u32 {
    DEFAULT_TOOLS_PORT
}

/// Sandbox user identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitUser {
    pub uid: u32,
    pub gid: u32,
    /// `HOME`; defaults to `default_cwd`.
    #[serde(default)]
    pub home: Option<PathBuf>,
    /// `USER` / `LOGNAME`, when known.
    #[serde(default)]
    pub name: Option<String>,
}

/// One workspace and where it is mounted in the guest.
///
/// Wire shape (flat object, `deny_unknown_fields`):
/// - block drive: `{"drive_id", "label"?, "guest_path", "read_only"?}`, optionally with
///   `"source": "block"`. A missing `source` means block, so configs written before
///   [`MountSource`] existed still parse, and a block mount is serialized **without**
///   `source` (byte-compatible with guests that predate it).
/// - virtio-fs share: `{"source": "virtio_fs", "tag", "guest_path", "read_only"?}`.
///
/// Fields of the other source kind are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "GuestMountWire", into = "GuestMountWire")]
pub struct GuestMount {
    /// Where the filesystem comes from.
    pub source: MountSource,
    /// Absolute guest mount point.
    pub guest_path: PathBuf,
    /// Mount read-only (`MS_RDONLY`; plus `noload` for block drives).
    pub read_only: bool,
}

impl GuestMount {
    /// A virtio-blk drive found by serial / ext4 label (Firecracker, libkrun block volumes).
    #[must_use]
    pub fn block(
        drive_id: impl Into<String>,
        label: Option<String>,
        guest_path: impl Into<PathBuf>,
        read_only: bool,
    ) -> Self {
        Self {
            source: MountSource::Block {
                drive_id: drive_id.into(),
                label,
            },
            guest_path: guest_path.into(),
            read_only,
        }
    }

    /// A virtio-fs share mounted by tag (libkrun host-dir workspaces).
    #[must_use]
    pub fn virtio_fs(
        tag: impl Into<String>,
        guest_path: impl Into<PathBuf>,
        read_only: bool,
    ) -> Self {
        Self {
            source: MountSource::VirtioFs { tag: tag.into() },
            guest_path: guest_path.into(),
            read_only,
        }
    }

    /// The block `drive_id`, or `None` for a virtio-fs share.
    #[must_use]
    pub fn drive_id(&self) -> Option<&str> {
        self.source.drive_id()
    }
}

/// Source of a [`GuestMount`].
///
/// Serialized on its own as an internally tagged object (`"source": "block" | "virtio_fs"`);
/// inside a [`GuestMount`] the fields are flattened and `source` defaults to `block`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum MountSource {
    /// virtio-blk drive with an ext4 filesystem.
    Block {
        /// Firecracker `drive_id` (`[A-Za-z0-9_]`, at most [`MAX_DRIVE_ID_LEN`] bytes). The
        /// guest finds the device by `/sys/block/vd*/serial`.
        drive_id: String,
        /// ext4 label to match when no device serial matches (defaults to `drive_id`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// virtio-fs share, mounted with `mount -t virtiofs <tag>`. The host VMM enforces
    /// read-only shares; the guest `MS_RDONLY` is defense in depth.
    VirtioFs {
        /// virtio-fs tag (at most [`MAX_VIRTIOFS_TAG_LEN`] bytes).
        tag: String,
    },
}

impl MountSource {
    /// The block `drive_id`, or `None` for a virtio-fs share.
    #[must_use]
    pub fn drive_id(&self) -> Option<&str> {
        match self {
            Self::Block { drive_id, .. } => Some(drive_id),
            Self::VirtioFs { .. } => None,
        }
    }

    fn kind(&self) -> MountSourceKind {
        match self {
            Self::Block { .. } => MountSourceKind::Block,
            Self::VirtioFs { .. } => MountSourceKind::VirtioFs,
        }
    }
}

/// The virtio-fs device config holds the tag in a 36-byte field.
pub const MAX_VIRTIOFS_TAG_LEN: usize = 36;

/// Discriminator of the flat [`GuestMount`] wire object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum MountSourceKind {
    #[default]
    Block,
    VirtioFs,
}

impl MountSourceKind {
    fn is_block(&self) -> bool {
        *self == Self::Block
    }
}

/// Flat wire form of [`GuestMount`] (and its JSON Schema).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "GuestMount")]
struct GuestMountWire {
    /// `block` (default when absent) or `virtio_fs`.
    #[serde(default, skip_serializing_if = "MountSourceKind::is_block")]
    source: MountSourceKind,
    /// Block only: Firecracker `drive_id` (`[A-Za-z0-9_]`, at most [`MAX_DRIVE_ID_LEN`]
    /// bytes). The guest finds the device by `/sys/block/vd*/serial`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drive_id: Option<String>,
    /// Block only: ext4 label to match when no device serial matches (defaults to `drive_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// virtio-fs only: the share tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    /// Absolute guest mount point.
    guest_path: PathBuf,
    /// Mount read-only.
    #[serde(default)]
    read_only: bool,
}

impl TryFrom<GuestMountWire> for GuestMount {
    type Error = String;

    fn try_from(w: GuestMountWire) -> Result<Self, String> {
        let source = match w.source {
            MountSourceKind::Block => {
                if w.tag.is_some() {
                    return Err("`tag` is only valid with `source: \"virtio_fs\"`".into());
                }
                MountSource::Block {
                    drive_id: w.drive_id.ok_or("block mount is missing `drive_id`")?,
                    label: w.label,
                }
            }
            MountSourceKind::VirtioFs => {
                if w.drive_id.is_some() || w.label.is_some() {
                    return Err(
                        "`drive_id` / `label` are only valid on block mounts, not virtio_fs".into(),
                    );
                }
                MountSource::VirtioFs {
                    tag: w.tag.ok_or("virtio_fs mount is missing `tag`")?,
                }
            }
        };
        Ok(Self {
            source,
            guest_path: w.guest_path,
            read_only: w.read_only,
        })
    }
}

impl From<GuestMount> for GuestMountWire {
    fn from(m: GuestMount) -> Self {
        let source = m.source.kind();
        let (drive_id, label, tag) = match m.source {
            MountSource::Block { drive_id, label } => (Some(drive_id), label, None),
            MountSource::VirtioFs { tag } => (None, None, Some(tag)),
        };
        Self {
            source,
            drive_id,
            label,
            tag,
            guest_path: m.guest_path,
            read_only: m.read_only,
        }
    }
}

impl JsonSchema for GuestMount {
    fn schema_name() -> String {
        GuestMountWire::schema_name()
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        GuestMountWire::json_schema(generator)
    }
}

/// `init.configure` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InitConfigureResult {
    /// PID of `crabd serve`.
    pub serve_pid: u32,
    /// Guest uptime when configuration finished (`/proc/uptime`), in ms.
    pub boot_ms: u64,
}

/// `init.export` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitExportParams {
    /// A writable drive from [`InitConfig::mounts`].
    pub drive_id: String,
}

/// `init.export` ack; the tar stream follows on the same connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InitExportAck {
    /// Always [`archive::FORMAT`].
    pub format: String,
    /// Guest mount point being exported.
    pub guest_path: PathBuf,
}

/// `init.shutdown` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitShutdownParams {
    /// How long processes get between SIGTERM and SIGKILL.
    #[serde(default = "default_grace_ms")]
    pub grace_ms: u64,
}

impl Default for InitShutdownParams {
    fn default() -> Self {
        Self {
            grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
        }
    }
}

fn default_grace_ms() -> u64 {
    DEFAULT_SHUTDOWN_GRACE_MS
}

/// `init.shutdown` result (empty object).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InitShutdownResult {}

/// The workspace archive format (`sandcrab-tar-v1`), shared by every backend that copies a
/// workspace in or out of a guest.
///
/// A POSIX/GNU tar stream (GNU long-name / long-link extensions allowed), terminated by the
/// standard end-of-archive marker. Producers (`crabd`'s `pack_tree`):
/// - emit entry paths **relative** to the workspace root, `/`-separated, never absolute,
///   never containing `..` or `.` components; the root itself is not an entry;
/// - emit only directories, regular files and symlinks (symlinks are stored, never followed);
///   sockets, FIFOs and devices are skipped; hard links are stored as independent files;
/// - emit a directory before its contents and siblings in byte order (deterministic);
/// - keep permission bits ([`PERMISSION_MASK`]), dropping setuid/setgid and file sticky bits;
/// - keep mtime; uid/gid are informational (consumers do not chown);
/// - stay on one filesystem (nested mounts are skipped) and skip `lost+found` at the root.
///
/// Consumers (`sandcrab-core::archive::unpack_safe`) reject anything else: absolute or `..`
/// paths, hard links, devices / FIFOs, setuid/setgid bits, writes through a symlink, duplicate
/// entries, a missing end-of-archive marker, and archives over the size / entry limits.
pub mod archive {
    /// Format id reported in [`super::InitExportAck::format`].
    pub const FORMAT: &str = "sandcrab-tar-v1";
    /// Permission bits kept for directories (sticky allowed on directories only).
    pub const DIR_PERMISSION_MASK: u32 = 0o1777;
    /// Permission bits kept for files and symlinks.
    pub const PERMISSION_MASK: u32 = 0o777;
    /// Directory at the workspace root that producers skip (created by `mke2fs`).
    pub const SKIPPED_ROOT_ENTRY: &str = "lost+found";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn configure_defaults_and_round_trip() {
        let cfg: InitConfig = serde_json::from_value(json!({
            "sandbox_id": "sb-1",
            "hostname": "sandcrab",
            "user": { "uid": 1000, "gid": 1000 },
            "default_cwd": "/workspace",
            "mounts": [{ "drive_id": "ws0", "guest_path": "/workspace" }],
        }))
        .unwrap();
        assert_eq!(cfg.tools_port, DEFAULT_TOOLS_PORT);
        assert!(!cfg.network && cfg.env.is_empty() && cfg.dns.is_empty());
        assert!(!cfg.mounts[0].read_only);
        assert_eq!(cfg.mounts[0].drive_id(), Some("ws0"));
        let back: InitConfig = serde_json::from_value(serde_json::to_value(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn configure_0_3_fields() {
        let base = json!({
            "sandbox_id": "sb-1",
            "hostname": "sandcrab",
            "user": { "uid": 1000, "gid": 1000 },
            "default_cwd": "/workspace",
        });
        // Without the new fields: defaults, and none of them is written back, so the config
        // stays byte-compatible with a guest that predates them.
        let cfg: InitConfig = serde_json::from_value(base.clone()).unwrap();
        assert!(!cfg.tmp_exec && cfg.forward_ports.is_empty());
        assert_eq!(
            (cfg.tmp_size_mb, cfg.pids_max, cfg.nofile),
            (None, None, None)
        );
        let v = serde_json::to_value(&cfg).unwrap();
        for k in [
            "tmp_exec",
            "tmp_size_mb",
            "pids_max",
            "nofile",
            "forward_ports",
        ] {
            assert!(v.get(k).is_none(), "{k}: {v}");
        }

        let mut full = base;
        full["tmp_exec"] = json!(true);
        full["tmp_size_mb"] = json!(1024);
        full["pids_max"] = json!(4096);
        full["nofile"] = json!(65536);
        full["forward_ports"] = json!([5432, 6379]);
        let cfg: InitConfig = serde_json::from_value(full.clone()).unwrap();
        assert!(cfg.tmp_exec);
        assert_eq!(cfg.tmp_size_mb, Some(1024));
        assert_eq!(cfg.pids_max, Some(4096));
        assert_eq!(cfg.nofile, Some(65536));
        assert_eq!(cfg.forward_ports, [5432, 6379]);
        let back = serde_json::to_value(&cfg).unwrap();
        for k in [
            "tmp_exec",
            "tmp_size_mb",
            "pids_max",
            "nofile",
            "forward_ports",
        ] {
            assert_eq!(back[k], full[k], "{k}");
        }

        full["forward_ports"] = json!([70000]);
        assert!(serde_json::from_value::<InitConfig>(full).is_err());
    }

    #[test]
    fn unknown_fields_rejected() {
        let err = serde_json::from_value::<InitShutdownParams>(json!({ "grace": 1 })).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
        let p: InitShutdownParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(p.grace_ms, DEFAULT_SHUTDOWN_GRACE_MS);
    }

    #[test]
    fn schemas_export() {
        let s = serde_json::to_value(schemars::schema_for!(InitConfig)).unwrap();
        assert!(s["properties"]["mounts"].is_object());
        let m = &s["definitions"]["GuestMount"];
        for field in [
            "source",
            "drive_id",
            "label",
            "tag",
            "guest_path",
            "read_only",
        ] {
            assert!(m["properties"][field].is_object(), "{field}: {m}");
        }
        assert_eq!(m["required"], json!(["guest_path"]));
        assert_eq!(m["additionalProperties"], json!(false));
    }

    /// Pre-`MountSource` wire shape (no `source`), as an old host or guest writes it.
    #[test]
    fn legacy_mount_is_block() {
        let m: GuestMount = serde_json::from_value(json!({
            "drive_id": "ws1", "label": null, "guest_path": "/docs", "read_only": true,
        }))
        .unwrap();
        assert_eq!(m, GuestMount::block("ws1", None, "/docs", true));
        let m: GuestMount = serde_json::from_value(json!({
            "drive_id": "ws0", "label": "data", "guest_path": "/w",
        }))
        .unwrap();
        assert_eq!(
            m.source,
            MountSource::Block {
                drive_id: "ws0".into(),
                label: Some("data".into())
            }
        );
        assert!(!m.read_only);
    }

    /// Block mounts serialize without `source`, so guests that predate it (and deny unknown
    /// fields) still accept configs from a new host.
    #[test]
    fn block_mount_serializes_in_legacy_shape() {
        let v = serde_json::to_value(GuestMount::block("ws0", None, "/workspace", false)).unwrap();
        assert_eq!(
            v,
            json!({ "drive_id": "ws0", "guest_path": "/workspace", "read_only": false })
        );
        let v =
            serde_json::to_value(GuestMount::block("ws0", Some("l".into()), "/w", true)).unwrap();
        assert_eq!(
            v,
            json!({ "drive_id": "ws0", "label": "l", "guest_path": "/w", "read_only": true })
        );
        // An explicit `source: block` is accepted too.
        let m: GuestMount = serde_json::from_value(json!({
            "source": "block", "drive_id": "ws0", "guest_path": "/workspace",
        }))
        .unwrap();
        assert_eq!(m, GuestMount::block("ws0", None, "/workspace", false));
    }

    #[test]
    fn virtio_fs_round_trip() {
        let m = GuestMount::virtio_fs("sc-ws0", "/workspace", true);
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(
            v,
            json!({ "source": "virtio_fs", "tag": "sc-ws0", "guest_path": "/workspace", "read_only": true })
        );
        assert_eq!(serde_json::from_value::<GuestMount>(v).unwrap(), m);
        assert_eq!(m.drive_id(), None);

        // Mixed block + virtio-fs config round-trips.
        let cfg: InitConfig = serde_json::from_value(json!({
            "sandbox_id": "sb-1",
            "hostname": "sandcrab",
            "user": { "uid": 501, "gid": 20 },
            "default_cwd": "/workspace",
            "mounts": [
                { "source": "virtio_fs", "tag": "ws0", "guest_path": "/workspace" },
                { "drive_id": "vol1", "guest_path": "/data", "read_only": true },
            ],
        }))
        .unwrap();
        assert_eq!(
            cfg.mounts[0],
            GuestMount::virtio_fs("ws0", "/workspace", false)
        );
        assert_eq!(
            cfg.mounts[1],
            GuestMount::block("vol1", None, "/data", true)
        );
        let back: InitConfig = serde_json::from_value(serde_json::to_value(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn mount_source_mismatches_rejected() {
        for (bad, why) in [
            (json!({ "guest_path": "/w" }), "missing `drive_id`"),
            (
                json!({ "source": "virtio_fs", "guest_path": "/w" }),
                "missing `tag`",
            ),
            (
                json!({ "drive_id": "ws0", "tag": "t", "guest_path": "/w" }),
                "`tag` is only valid",
            ),
            (
                json!({ "source": "virtio_fs", "tag": "t", "drive_id": "ws0", "guest_path": "/w" }),
                "only valid on block",
            ),
            (
                json!({ "source": "virtio_fs", "tag": "t", "label": "l", "guest_path": "/w" }),
                "only valid on block",
            ),
            (
                json!({ "source": "nfs", "guest_path": "/w" }),
                "unknown variant",
            ),
            (
                json!({ "drive_id": "ws0", "guest_path": "/w", "ro": true }),
                "unknown field",
            ),
        ] {
            let err = serde_json::from_value::<GuestMount>(bad.clone()).unwrap_err();
            assert!(err.to_string().contains(why), "{bad}: {err}");
        }
    }

    #[test]
    fn mount_source_standalone_tagging() {
        let s = MountSource::VirtioFs { tag: "t".into() };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v, json!({ "source": "virtio_fs", "tag": "t" }));
        assert_eq!(serde_json::from_value::<MountSource>(v).unwrap(), s);
        let b: MountSource =
            serde_json::from_value(json!({ "source": "block", "drive_id": "ws0" })).unwrap();
        assert_eq!(b.drive_id(), Some("ws0"));
    }

    #[test]
    fn hardening_sysctls_are_well_formed() {
        let mut keys = std::collections::BTreeSet::new();
        for s in HARDENING_SYSCTLS {
            assert!(keys.insert(s.key), "duplicate {}", s.key);
            assert!(!s.values.is_empty(), "{}", s.key);
            assert!(
                s.key.split('.').all(|p| !p.is_empty()
                    && p.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')),
                "{}",
                s.key
            );
            for v in s.values {
                assert!(v.parse::<i64>().is_ok(), "{}={v}", s.key);
            }
        }
        for p in PROC_READONLY_PATHS {
            assert!(p.starts_with("/proc/") && !p.contains(".."), "{p}");
        }
    }
}
