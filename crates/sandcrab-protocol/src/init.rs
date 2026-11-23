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
    /// Workspace drives to mount, in any order (the guest mounts parents first).
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

/// One workspace drive and where it is mounted in the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuestMount {
    /// Firecracker `drive_id` (`[A-Za-z0-9_]`, at most [`MAX_DRIVE_ID_LEN`] bytes). The guest
    /// finds the device by `/sys/block/vd*/serial`.
    pub drive_id: String,
    /// ext4 label to match when no device serial matches (defaults to `drive_id`).
    #[serde(default)]
    pub label: Option<String>,
    /// Absolute guest mount point.
    pub guest_path: PathBuf,
    /// Mount read-only (`MS_RDONLY` + `noload`).
    #[serde(default)]
    pub read_only: bool,
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
        let back: InitConfig = serde_json::from_value(serde_json::to_value(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
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
    }
}
