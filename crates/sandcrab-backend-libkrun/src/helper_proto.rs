//! Backend → `sandcrab-krun` helper protocol (design §2): one [`HelperConfig`] as a single
//! line of compact JSON on the helper's stdin, after which the pipe stays open as the
//! lifeline (EOF = the owning process is gone → the helper kills itself).
//!
//! Shared by the backend (which builds, validates and writes it) and the helper (which reads,
//! re-validates and applies it). Validation is strict because libkrun **aborts the whole
//! process** on some bad input (a newline in argv/env panics in its cmdline builder), and
//! because every string ends up in a C string (no NUL).
//!
//! String rules:
//! - Strings that reach the **guest kernel cmdline** (exec path, args, env, workdir) must be
//!   printable ASCII without `"` or `\` (libkrun quotes them), at most
//!   [`MAX_CMDLINE_STRING`] bytes.
//! - **Host paths** must be absolute, normalized (no `.`/`..`), and free of NUL and control
//!   characters; non-ASCII UTF-8 is allowed (they never reach the cmdline). Socket paths must
//!   fit the macOS `sun_path` ([`MAX_SOCKET_PATH`] bytes, never truncated).
//! - **Device ids** (virtio-blk serials, virtio-fs tags) are short `[A-Za-z0-9_-]` names.

use std::collections::HashSet;
use std::io::BufRead;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::netstack::{Ipv4Net, MAX_DENY_PREFIXES, NetstackKind};

pub use sandcrab_rootfs::FileStamp;

pub use sandcrab_protocol::init::{
    CONTROL_PORT, DEFAULT_TOOLS_PORT as TOOLS_PORT, ROOT_DRIVE_ID, SCRATCH_DRIVE_ID,
};

/// Version of the [`HelperConfig`] wire format. The helper refuses any other version.
/// 2: stat stamps on pinned files and disks, the pinned `passt.avx2` variant.
pub const HELPER_PROTO_VERSION: u32 = 2;
/// Upper bound on the config line (including the trailing newline).
pub const MAX_HELPER_CONFIG_BYTES: usize = 64 * 1024;
/// Longest Unix socket path, in bytes without the NUL: macOS `sun_path` is 104 bytes (Linux
/// 108). The stricter macOS limit applies everywhere so configs are portable.
pub const MAX_SOCKET_PATH: usize = 103;
/// Longest host path (macOS `PATH_MAX` is 1024 including the NUL).
pub const MAX_HOST_PATH: usize = 1023;
/// virtio-blk `GET_ID` serial size: the guest resolves drives by this serial.
pub const MAX_BLOCK_ID: usize = 20;
/// virtio-fs tag size in the device config space.
pub const MAX_VIRTIOFS_TAG: usize = 36;
/// Longest single string that lands on the guest kernel cmdline.
pub const MAX_CMDLINE_STRING: usize = 256;
/// Block-volume workspace disks besides root and scratch.
pub const MAX_EXTRA_DISKS: usize = 8;
/// virtio-fs shares (one per host-dir workspace mount).
pub const MAX_VIRTIOFS_SHARES: usize = 16;
/// Guest args / env entries (all fixed by the backend today).
pub const MAX_GUEST_ARGS: usize = 16;
pub const MAX_GUEST_ENV: usize = 16;
/// Guest RAM bounds in MiB.
pub const MIN_MEMORY_MIB: u32 = 128;
pub const MAX_MEMORY_MIB: u32 = 512 * 1024;
/// Highest libkrun log level (`krun_set_log_level`: 0 = off … 5 = trace).
pub const MAX_LOG_LEVEL: u32 = 5;

/// Guest init the helper execs as PID 1 (`KRUN_INIT_PID1=1`, design §6).
pub const GUEST_INIT_PATH: &str = "/sbin/crabd";
/// Default guest `PATH`.
pub const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Helper exit codes for failures **before** `krun_start_enter` (design §2). Codes 125, 126
/// and 127 come from libkrun itself and mean "guest init failed".
pub mod exit_code {
    /// The config on stdin could not be read, parsed or validated.
    pub const CONFIG_INVALID: i32 = 70;
    /// libkrun, libkrunfw or the root disk did not match its pinned SHA-256.
    pub const PIN_MISMATCH: i32 = 71;
    /// A workspace root's (dev, ino) no longer matches the lease identity.
    pub const WORKSPACE_IDENTITY: i32 = 72;
    /// A `krun_*` configuration call failed.
    pub const LIBKRUN_CONFIG: i32 = 73;
    /// The pinned library could not be loaded or lacks a required symbol.
    pub const LIBKRUN_LOAD: i32 = 74;
    /// `krun_start_enter` returned (it only returns on error, e.g. a missing entitlement).
    pub const START_FAILED: i32 = 75;
    /// The userspace netstack (passt / gvproxy) could not be started.
    pub const NETSTACK_START: i32 = 76;

    /// Human-readable meaning of a helper exit code, if it is one of ours or libkrun's.
    #[must_use]
    pub fn describe(code: i32) -> Option<&'static str> {
        Some(match code {
            CONFIG_INVALID => "invalid helper config",
            PIN_MISMATCH => "pinned artifact SHA-256 mismatch",
            WORKSPACE_IDENTITY => "workspace root identity changed",
            LIBKRUN_CONFIG => "libkrun configuration call failed",
            LIBKRUN_LOAD => "failed to load the pinned libkrun",
            START_FAILED => "krun_start_enter failed (missing com.apple.security.hypervisor?)",
            NETSTACK_START => "the userspace netstack (passt / gvproxy) failed to start",
            125..=127 => "guest init failed",
            _ => return None,
        })
    }
}

/// Errors reading or validating a [`HelperConfig`].
#[derive(Debug, thiserror::Error)]
pub enum HelperConfigError {
    #[error("reading helper config: {0}")]
    Io(#[from] std::io::Error),
    #[error("helper config exceeds {limit} bytes")]
    TooLarge { limit: usize },
    #[error("helper config is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("helper config version {got} is not supported (expected {expected})")]
    Version { got: u32, expected: u32 },
    #[error("invalid helper config field `{field}`: {reason}")]
    Invalid { field: String, reason: String },
}

fn invalid(field: impl Into<String>, reason: impl Into<String>) -> HelperConfigError {
    HelperConfigError::Invalid {
        field: field.into(),
        reason: reason.into(),
    }
}

type Result<T> = std::result::Result<T, HelperConfigError>;

/// Everything the helper needs to build and start one VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperConfig {
    /// Must equal [`HELPER_PROTO_VERSION`].
    pub version: u32,
    /// libkrun dylib/so: verified (stamp, else SHA-256) through one fd, `dlopen`ed through it.
    pub libkrun: PinnedFile,
    /// libkrunfw (the bundled guest kernel): verified and `dlopen`ed through its fd first, so
    /// libkrun's later leaf-name `dlopen` resolves to that loaded image (checked with
    /// `RTLD_NOLOAD`, see `ffi.rs`).
    pub libkrunfw: PinnedFile,
    /// 1..=255 (`krun_set_vm_config` takes a `u8`; the helper also checks
    /// `krun_get_max_vcpus`).
    pub vcpus: u8,
    pub memory_mib: u32,
    /// Read-only ext4 root (`/dev/vda`, serial [`ROOT_DRIVE_ID`]), remounted by `init.krun`.
    pub root: DiskSpec,
    /// Per-sandbox writable scratch disk (serial [`SCRATCH_DRIVE_ID`], overlay upper).
    pub scratch: DiskSpec,
    /// Block-volume workspace disks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<DiskSpec>,
    /// Live host-dir workspace shares.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub virtiofs: Vec<VirtioFsShare>,
    #[serde(default)]
    pub virtiofs_semantics: VirtiofsSemantics,
    /// Host socket for guest vsock [`CONTROL_PORT`] (guest listens, `listen=true`).
    pub control_socket: PathBuf,
    /// Host socket for guest vsock [`TOOLS_PORT`] (`crabd serve`).
    pub tools_socket: PathBuf,
    #[serde(default)]
    pub network: HelperNetwork,
    /// The netstack for [`HelperNetwork::Userspace`] (required then, absent otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub netstack: Option<NetstackSpec>,
    #[serde(default)]
    pub exec: GuestExec,
    /// libkrun log level, 0 (off) ..= [`MAX_LOG_LEVEL`].
    #[serde(default)]
    pub log_level: u32,
}

/// A host file pinned by SHA-256 (lowercase hex), plus the stat stamp recorded with that
/// digest (`sandcrab_rootfs::stamp`): the helper opens the file once, and a matching stamp
/// spares it the hash; otherwise it hashes the fd. It then uses the fd, never the path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedFile {
    pub path: PathBuf,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamp: Option<FileStamp>,
}

impl PinnedFile {
    /// A pin without a stamp (the helper hashes it).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, sha256: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            sha256: sha256.into(),
            stamp: None,
        }
    }
}

/// A raw disk image attached with `krun_add_disk`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskSpec {
    /// virtio-blk serial the guest resolves the drive by.
    pub block_id: String,
    pub path: PathBuf,
    pub read_only: bool,
    /// SHA-256 of the image (the shared rootfs). With a [`Self::stamp`] the helper hashes only
    /// when the stamp does not match; without one, on every spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Stat stamp the backend verified the image with. When set, the helper opens the disk
    /// once, requires this stamp (or, on a mismatch, the [`Self::sha256`]) and hands libkrun
    /// that fd (`/proc/self/fd/<n>` | `/dev/fd/<n>`) instead of the path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamp: Option<FileStamp>,
}

/// One `krun_add_virtiofs3` share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VirtioFsShare {
    /// virtio-fs tag the guest mounts (`MountSource::VirtioFs { tag }`).
    pub tag: String,
    /// Canonical host directory (the leased workspace root).
    pub host_path: PathBuf,
    /// Enforced by libkrun's host-side server (guest remount cannot lift it).
    pub read_only: bool,
    /// Lease identity of `host_path`, re-checked right before the share is added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<HostIdentity>,
}

/// `(st_dev, st_ino)` of a host directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// How libkrun's virtio-fs server maps guest ownership (design §8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtiofsSemantics {
    /// Host files appear with their host uid; guest-only metadata lives in xattrs.
    #[default]
    LinuxComplete,
    /// Every guest uid owns everything; mode bits are written to the real host files.
    LinuxSimplified,
}

/// Guest networking the helper sets up (design §9). TSI is never enabled implicitly; the
/// port map is always empty.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperNetwork {
    /// `krun_disable_implicit_vsock` + `krun_add_vsock(0)`: vsock without TSI, no NIC.
    #[default]
    Disabled,
    /// Explicit (deprecated) operator opt-in: TSI (`KRUN_TSI_HIJACK_INET`) with an empty port
    /// map. The guest can reach host loopback services.
    TsiUnrestricted,
    /// Phase B: a virtio-net NIC (`krun_add_net_unixstream`, DHCP by `init.krun`) backed by the
    /// pinned userspace netstack in [`HelperConfig::netstack`], behind the helper's egress
    /// filter ([`crate::netstack`]). TSI stays off.
    Userspace,
}

/// The userspace netstack the helper spawns for [`HelperNetwork::Userspace`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetstackSpec {
    pub kind: NetstackKind,
    /// The netstack executable, verified by the helper through an fd right before it is
    /// spawned (Linux: executed through that fd).
    pub binary: PinnedFile,
    /// passt's AVX2 build (`passt.avx2`), when installed. passt re-execs
    /// `<readlink /proc/self/exe>.avx2` by path on AVX2 CPUs; the helper instead executes this
    /// pinned variant itself on such CPUs (passt then does not re-exec), and refuses to start
    /// plain passt next to an unpinned `.avx2` file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avx2: Option<PinnedFile>,
    /// Destinations denied on top of [`crate::netstack::ALWAYS_DENIED`]: the host's own
    /// addresses and on-link subnets ([`crate::netstack::host_prefixes`]).
    #[serde(default)]
    pub deny: Vec<Ipv4Net>,
}

/// The guest process `init.krun` execs. Every string lands on the kernel cmdline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestExec {
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// `KEY=VALUE` entries.
    #[serde(default)]
    pub env: Vec<String>,
    pub workdir: String,
}

impl Default for GuestExec {
    /// `crabd init` as PID 1 (design §6): fixed argv and env, real config over the control
    /// port.
    fn default() -> Self {
        Self {
            path: GUEST_INIT_PATH.into(),
            args: vec!["init".into()],
            env: vec!["KRUN_INIT_PID1=1".into(), format!("PATH={GUEST_PATH}")],
            workdir: "/".into(),
        }
    }
}

impl HelperConfig {
    /// Read one config line (at most [`MAX_HELPER_CONFIG_BYTES`]) and validate it. Does not
    /// consume anything after the newline, so the caller can keep reading the same stream as
    /// the lifeline.
    pub fn read_from<R: BufRead>(reader: R) -> Result<Self> {
        let mut line = Vec::new();
        reader
            .take(MAX_HELPER_CONFIG_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if line.len() > MAX_HELPER_CONFIG_BYTES {
            return Err(HelperConfigError::TooLarge {
                limit: MAX_HELPER_CONFIG_BYTES,
            });
        }
        // Check the version before the full parse so an old/new peer gets a clear error.
        #[derive(Deserialize)]
        struct VersionOnly {
            version: u32,
        }
        let v: VersionOnly = serde_json::from_slice(&line)?;
        check_version(v.version)?;
        let cfg: Self = serde_json::from_slice(&line)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate, then serialize as one line of compact JSON terminated by `\n` (JSON string
    /// escaping guarantees no raw newline inside).
    pub fn to_line(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut line = serde_json::to_vec(self)?;
        line.push(b'\n');
        if line.len() > MAX_HELPER_CONFIG_BYTES {
            return Err(HelperConfigError::TooLarge {
                limit: MAX_HELPER_CONFIG_BYTES,
            });
        }
        Ok(line)
    }

    /// Check every invariant the helper relies on before it touches libkrun.
    pub fn validate(&self) -> Result<()> {
        check_version(self.version)?;
        self.libkrun.validate("libkrun")?;
        self.libkrunfw.validate("libkrunfw")?;

        if self.vcpus == 0 {
            return Err(invalid("vcpus", "must be at least 1"));
        }
        if !(MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&self.memory_mib) {
            return Err(invalid(
                "memory_mib",
                format!("must be within {MIN_MEMORY_MIB}..={MAX_MEMORY_MIB}"),
            ));
        }
        if self.log_level > MAX_LOG_LEVEL {
            return Err(invalid("log_level", format!("must be ≤ {MAX_LOG_LEVEL}")));
        }

        // Disks: root first (it becomes /dev/vda), then scratch, then workspaces.
        self.root.validate("root")?;
        self.scratch.validate("scratch")?;
        if self.root.block_id != ROOT_DRIVE_ID || !self.root.read_only {
            return Err(invalid(
                "root",
                format!("must be read-only with block_id `{ROOT_DRIVE_ID}`"),
            ));
        }
        if self.scratch.block_id != SCRATCH_DRIVE_ID || self.scratch.read_only {
            return Err(invalid(
                "scratch",
                format!("must be writable with block_id `{SCRATCH_DRIVE_ID}`"),
            ));
        }
        if self.disks.len() > MAX_EXTRA_DISKS {
            return Err(invalid("disks", format!("at most {MAX_EXTRA_DISKS}")));
        }
        let mut block_ids = HashSet::new();
        let mut disk_paths = HashSet::new();
        for (i, d) in [&self.root, &self.scratch]
            .into_iter()
            .chain(&self.disks)
            .enumerate()
        {
            if i >= 2 {
                d.validate(&format!("disks[{}]", i - 2))?;
            }
            if !block_ids.insert(d.block_id.as_str()) {
                return Err(invalid(
                    "disks",
                    format!("duplicate block_id `{}`", d.block_id),
                ));
            }
            if !disk_paths.insert(d.path.as_path()) {
                return Err(invalid(
                    "disks",
                    format!("disk image `{}` attached twice", d.path.display()),
                ));
            }
        }

        if self.virtiofs.len() > MAX_VIRTIOFS_SHARES {
            return Err(invalid(
                "virtiofs",
                format!("at most {MAX_VIRTIOFS_SHARES}"),
            ));
        }
        let mut tags = HashSet::new();
        for (i, s) in self.virtiofs.iter().enumerate() {
            let field = format!("virtiofs[{i}]");
            check_device_id(&format!("{field}.tag"), &s.tag, MAX_VIRTIOFS_TAG)?;
            check_host_path(&format!("{field}.host_path"), &s.host_path, MAX_HOST_PATH)?;
            if !tags.insert(s.tag.as_str()) {
                return Err(invalid("virtiofs", format!("duplicate tag `{}`", s.tag)));
            }
        }

        match (self.network, &self.netstack) {
            (HelperNetwork::Userspace, Some(ns)) => {
                ns.binary.validate("netstack.binary")?;
                if let Some(v) = &ns.avx2 {
                    if ns.kind != NetstackKind::Passt {
                        return Err(invalid("netstack.avx2", "only passt has an AVX2 variant"));
                    }
                    v.validate("netstack.avx2")?;
                }
                if ns.deny.len() > MAX_DENY_PREFIXES {
                    return Err(invalid(
                        "netstack.deny",
                        format!("at most {MAX_DENY_PREFIXES} prefixes"),
                    ));
                }
            }
            (HelperNetwork::Userspace, None) => {
                return Err(invalid("netstack", "required for network `userspace`"));
            }
            (_, Some(_)) => {
                return Err(invalid("netstack", "only valid with network `userspace`"));
            }
            (_, None) => {}
        }

        check_host_path("control_socket", &self.control_socket, MAX_SOCKET_PATH)?;
        check_host_path("tools_socket", &self.tools_socket, MAX_SOCKET_PATH)?;
        if self.control_socket == self.tools_socket {
            return Err(invalid("tools_socket", "must differ from control_socket"));
        }

        self.exec.validate()
    }
}

impl PinnedFile {
    fn validate(&self, field: &str) -> Result<()> {
        check_host_path(&format!("{field}.path"), &self.path, MAX_HOST_PATH)?;
        check_sha256(&format!("{field}.sha256"), &self.sha256)
    }
}

impl DiskSpec {
    fn validate(&self, field: &str) -> Result<()> {
        check_device_id(&format!("{field}.block_id"), &self.block_id, MAX_BLOCK_ID)?;
        check_host_path(&format!("{field}.path"), &self.path, MAX_HOST_PATH)?;
        if let Some(sha) = &self.sha256 {
            check_sha256(&format!("{field}.sha256"), sha)?;
        }
        Ok(())
    }
}

impl GuestExec {
    fn validate(&self) -> Result<()> {
        check_cmdline_string("exec.path", &self.path)?;
        if !self.path.starts_with('/') {
            return Err(invalid("exec.path", "must be an absolute guest path"));
        }
        check_cmdline_string("exec.workdir", &self.workdir)?;
        if !self.workdir.starts_with('/') {
            return Err(invalid("exec.workdir", "must be an absolute guest path"));
        }
        if self.args.len() > MAX_GUEST_ARGS {
            return Err(invalid("exec.args", format!("at most {MAX_GUEST_ARGS}")));
        }
        for (i, a) in self.args.iter().enumerate() {
            check_cmdline_string(&format!("exec.args[{i}]"), a)?;
        }
        if self.env.len() > MAX_GUEST_ENV {
            return Err(invalid("exec.env", format!("at most {MAX_GUEST_ENV}")));
        }
        let mut keys = HashSet::new();
        for (i, e) in self.env.iter().enumerate() {
            let field = format!("exec.env[{i}]");
            check_cmdline_string(&field, e)?;
            let (key, _) = e
                .split_once('=')
                .ok_or_else(|| invalid(&field, "must be KEY=VALUE"))?;
            let mut chars = key.chars();
            let valid_key = chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid_key {
                return Err(invalid(&field, format!("invalid variable name `{key}`")));
            }
            if !keys.insert(key) {
                return Err(invalid(&field, format!("duplicate variable `{key}`")));
            }
        }
        Ok(())
    }
}

fn check_version(got: u32) -> Result<()> {
    if got == HELPER_PROTO_VERSION {
        Ok(())
    } else {
        Err(HelperConfigError::Version {
            got,
            expected: HELPER_PROTO_VERSION,
        })
    }
}

/// Printable ASCII without `"` and `\`, non-empty, bounded: safe inside libkrun's quoted
/// kernel cmdline (a newline there aborts the process).
fn check_cmdline_string(field: &str, s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    if s.len() > MAX_CMDLINE_STRING {
        return Err(invalid(
            field,
            format!("longer than {MAX_CMDLINE_STRING} bytes"),
        ));
    }
    if let Some(c) = s
        .chars()
        .find(|&c| !(' '..='~').contains(&c) || c == '"' || c == '\\')
    {
        return Err(invalid(
            field,
            format!(
                "contains {c:?} (only printable ASCII without `\"` or `\\` reaches the kernel cmdline)"
            ),
        ));
    }
    Ok(())
}

/// virtio device ids: `[A-Za-z0-9_-]{1,max}`.
fn check_device_id(field: &str, s: &str, max: usize) -> Result<()> {
    if s.is_empty() || s.len() > max {
        return Err(invalid(field, format!("must be 1..={max} bytes")));
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(invalid(field, "only [A-Za-z0-9_-] allowed"));
    }
    Ok(())
}

/// Absolute, normalized, no NUL/control characters, at most `max` bytes.
fn check_host_path(field: &str, p: &Path, max: usize) -> Result<()> {
    let len = p.as_os_str().len();
    if len > max {
        return Err(invalid(
            field,
            format!("path is {len} bytes, limit is {max} (never truncated)"),
        ));
    }
    let s = p
        .to_str()
        .ok_or_else(|| invalid(field, "path is not valid UTF-8"))?;
    if s.chars().any(char::is_control) {
        return Err(invalid(field, "path contains NUL or control characters"));
    }
    if !p.is_absolute() {
        return Err(invalid(field, "path must be absolute"));
    }
    if p.components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        || s.contains("/./")
        || s.ends_with("/.")
    {
        return Err(invalid(field, "path must be normalized (no `.` or `..`)"));
    }
    Ok(())
}

fn check_sha256(field: &str, s: &str) -> Result<()> {
    if s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(())
    } else {
        Err(invalid(field, "must be 64 lowercase hex characters"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    const SHA_A: &str = "3c2ffda8f326e6cdcbb504d8e7dc450f84dca8c57b001fce7b6bc6dbdaca5b5a";
    const SHA_B: &str = "1ccb5283921f36fedaa8f85ba6265e298a6b86d56be49f642c768b32102b91df";

    fn sample() -> HelperConfig {
        HelperConfig {
            version: HELPER_PROTO_VERSION,
            libkrun: PinnedFile {
                path: "/h/.sandcrab/artifacts/libkrun/1.19.8/libkrun.dylib".into(),
                sha256: SHA_A.into(),
                stamp: None,
            },
            libkrunfw: PinnedFile {
                path: "/h/.sandcrab/artifacts/libkrun/1.19.8/libkrunfw.5.dylib".into(),
                sha256: SHA_B.into(),
                stamp: None,
            },
            vcpus: 2,
            memory_mib: 1024,
            root: DiskSpec {
                block_id: ROOT_DRIVE_ID.into(),
                path: "/h/.sandcrab/cache/rootfs/abc/rootfs.ext4".into(),
                read_only: true,
                sha256: Some(SHA_A.into()),
                stamp: None,
            },
            scratch: DiskSpec {
                block_id: SCRATCH_DRIVE_ID.into(),
                path: "/h/.sandcrab/run/krun/id/scratch.ext4".into(),
                read_only: false,
                sha256: None,
                stamp: None,
            },
            disks: vec![],
            virtiofs: vec![
                VirtioFsShare {
                    tag: "ws0".into(),
                    host_path: "/Users/josé/projects/app".into(),
                    read_only: false,
                    identity: Some(HostIdentity {
                        dev: 16_777_234,
                        ino: 42,
                    }),
                },
                VirtioFsShare {
                    tag: "ws1".into(),
                    host_path: "/data/ref".into(),
                    read_only: true,
                    identity: None,
                },
            ],
            virtiofs_semantics: VirtiofsSemantics::LinuxComplete,
            control_socket: "/h/.sandcrab/run/krun/id/ctl.sock".into(),
            tools_socket: "/h/.sandcrab/run/krun/id/v.sock".into(),
            network: HelperNetwork::Disabled,
            netstack: None,
            exec: GuestExec::default(),
            log_level: 0,
        }
    }

    fn userspace() -> HelperConfig {
        let mut cfg = sample();
        cfg.network = HelperNetwork::Userspace;
        cfg.netstack = Some(NetstackSpec {
            kind: NetstackKind::Passt,
            binary: PinnedFile {
                path: "/h/.sandcrab/artifacts/libkrun-net/passt/passt".into(),
                sha256: SHA_A.into(),
                stamp: None,
            },
            deny: vec![
                "203.0.113.7/32".parse().unwrap(),
                "198.51.100.0/24".parse().unwrap(),
            ],
            avx2: None,
        });
        cfg
    }

    #[test]
    fn userspace_network_needs_exactly_one_netstack() {
        let cfg = userspace();
        cfg.validate().unwrap();
        let back = HelperConfig::read_from(Cursor::new(cfg.to_line().unwrap())).unwrap();
        assert_eq!(back, cfg);
        let v = serde_json::to_value(&cfg).unwrap();
        assert_eq!(v["network"], "userspace");
        assert_eq!(v["netstack"]["kind"], "passt");
        assert_eq!(v["netstack"]["deny"][0], "203.0.113.7/32");

        let mut c = userspace();
        c.netstack = None;
        assert_invalid(&c, "netstack");
        for net in [HelperNetwork::Disabled, HelperNetwork::TsiUnrestricted] {
            let mut c = userspace();
            c.network = net;
            assert_invalid(&c, "netstack");
        }
        let mut c = userspace();
        c.netstack.as_mut().unwrap().binary.path = "passt".into();
        assert_invalid(&c, "netstack.binary.path");
        let mut c = userspace();
        c.netstack.as_mut().unwrap().deny =
            vec!["10.0.0.0/8".parse().unwrap(); MAX_DENY_PREFIXES + 1];
        assert_invalid(&c, "netstack.deny");
        // A prefix with host bits set never parses, nor does an unknown netstack.
        let mut v = serde_json::to_value(userspace()).unwrap();
        v["netstack"]["deny"][0] = "10.0.0.1/8".into();
        assert!(serde_json::from_value::<HelperConfig>(v).is_err());
        let mut v = serde_json::to_value(userspace()).unwrap();
        v["netstack"]["kind"] = "slirp".into();
        assert!(serde_json::from_value::<HelperConfig>(v).is_err());
    }

    fn assert_invalid(cfg: &HelperConfig, field_prefix: &str) {
        match cfg.validate() {
            Err(HelperConfigError::Invalid { field, .. }) => assert!(
                field.starts_with(field_prefix),
                "expected field `{field_prefix}…`, got `{field}`"
            ),
            other => panic!("expected Invalid({field_prefix}), got {other:?}"),
        }
    }

    #[test]
    fn sample_is_valid_and_round_trips() {
        let cfg = sample();
        cfg.validate().unwrap();
        let line = cfg.to_line().unwrap();
        assert_eq!(line.last(), Some(&b'\n'));
        assert_eq!(line.iter().filter(|&&b| b == b'\n').count(), 1);
        let back = HelperConfig::read_from(Cursor::new(&line)).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn wire_shape() {
        let v = serde_json::to_value(sample()).unwrap();
        assert_eq!(v["network"], "disabled");
        assert_eq!(v["virtiofs_semantics"], "linux_complete");
        assert_eq!(v["exec"]["path"], "/sbin/crabd");
        assert_eq!(v["exec"]["args"], serde_json::json!(["init"]));
        assert!(v.get("disks").is_none(), "empty disks are omitted");
        assert!(v["scratch"].get("sha256").is_none());
        let mut tsi = sample();
        tsi.network = HelperNetwork::TsiUnrestricted;
        assert_eq!(
            serde_json::to_value(tsi).unwrap()["network"],
            "tsi_unrestricted"
        );
    }

    #[test]
    fn defaults_apply_for_optional_fields() {
        let mut v = serde_json::to_value(sample()).unwrap();
        let o = v.as_object_mut().unwrap();
        for k in [
            "virtiofs",
            "virtiofs_semantics",
            "network",
            "netstack",
            "exec",
            "log_level",
        ] {
            o.remove(k);
        }
        let cfg: HelperConfig = serde_json::from_value(v).unwrap();
        assert_eq!(cfg.network, HelperNetwork::Disabled);
        assert_eq!(cfg.exec, GuestExec::default());
        assert!(cfg.exec.env.contains(&"KRUN_INIT_PID1=1".to_string()));
        assert!(cfg.virtiofs.is_empty());
        cfg.validate().unwrap();
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut v = serde_json::to_value(sample()).unwrap();
        v["port_map"] = serde_json::json!(["8080:8080"]);
        assert!(serde_json::from_value::<HelperConfig>(v).is_err());

        let mut v = serde_json::to_value(sample()).unwrap();
        v["virtiofs"][0]["writeback"] = true.into();
        assert!(serde_json::from_value::<HelperConfig>(v).is_err());

        let mut v = serde_json::to_value(sample()).unwrap();
        v["network"] = "tsi".into();
        assert!(serde_json::from_value::<HelperConfig>(v).is_err());
    }

    #[test]
    fn version_mismatch() {
        let mut cfg = sample();
        cfg.version = 1;
        assert!(matches!(
            cfg.validate(),
            Err(HelperConfigError::Version {
                got: 1,
                expected: HELPER_PROTO_VERSION
            })
        ));
        // Checked before the full parse: a future config with new fields still reports the
        // version, not an unknown field.
        let mut v = serde_json::to_value(sample()).unwrap();
        v["version"] = 9.into();
        v["something_new"] = true.into();
        let line = format!("{v}\n");
        assert!(matches!(
            HelperConfig::read_from(Cursor::new(line)),
            Err(HelperConfigError::Version { got: 9, .. })
        ));
    }

    #[test]
    fn newlines_and_quotes_never_reach_the_cmdline() {
        for bad in [
            "a\nb",
            "a\rb",
            "a\0b",
            "say \"hi\"",
            "a\\b",
            "tab\t",
            "é",
            "",
        ] {
            let mut cfg = sample();
            cfg.exec.args = vec!["init".into(), bad.into()];
            assert_invalid(&cfg, "exec.args[1]");

            let mut cfg = sample();
            cfg.exec.env.push(format!("X={bad}"));
            if !bad.is_empty() {
                assert_invalid(&cfg, "exec.env[2]");
            }
        }
        let mut cfg = sample();
        cfg.exec.args.push("with space ok".into());
        cfg.validate().unwrap();
        cfg.exec.args.push("x".repeat(MAX_CMDLINE_STRING + 1));
        assert_invalid(&cfg, "exec.args");
    }

    #[test]
    fn env_shape() {
        for bad in ["NOEQUALS", "=v", "1X=v", "A-B=v"] {
            let mut cfg = sample();
            cfg.exec.env.push(bad.into());
            assert_invalid(&cfg, "exec.env");
        }
        let mut cfg = sample();
        cfg.exec.env.push("PATH=/bin".into());
        assert_invalid(&cfg, "exec.env[2]");
        let mut cfg = sample();
        cfg.exec.env.push("EMPTY=".into());
        cfg.validate().unwrap();
        let mut cfg = sample();
        cfg.exec.path = "sbin/crabd".into();
        assert_invalid(&cfg, "exec.path");
        let mut cfg = sample();
        cfg.exec.workdir = "tmp".into();
        assert_invalid(&cfg, "exec.workdir");
        let mut cfg = sample();
        cfg.exec.env = (0..=MAX_GUEST_ENV).map(|i| format!("V{i}=1")).collect();
        assert_invalid(&cfg, "exec.env");
    }

    #[test]
    fn socket_path_budget() {
        let dir = "/s";
        let fill = |total: usize| {
            let name_len = total - dir.len() - 1;
            PathBuf::from(format!("{dir}/{}", "a".repeat(name_len)))
        };
        let mut cfg = sample();
        cfg.control_socket = fill(MAX_SOCKET_PATH);
        assert_eq!(cfg.control_socket.as_os_str().len(), 103);
        cfg.validate().unwrap();
        cfg.control_socket = fill(MAX_SOCKET_PATH + 1);
        assert_invalid(&cfg, "control_socket");

        let mut cfg = sample();
        cfg.tools_socket = cfg.control_socket.clone();
        assert_invalid(&cfg, "tools_socket");
    }

    #[test]
    fn host_paths() {
        for bad in [
            "relative/disk.ext4",
            "/a/../b",
            "/a/./b",
            "/a/b/.",
            "/a\nb",
            "/a\0b",
            "/a\u{7f}b",
        ] {
            let mut cfg = sample();
            cfg.scratch.path = bad.into();
            assert_invalid(&cfg, "scratch.path");
        }
        let mut cfg = sample();
        cfg.virtiofs[0].host_path = format!("/{}", "d".repeat(MAX_HOST_PATH)).into();
        assert_invalid(&cfg, "virtiofs[0].host_path");
        let mut cfg = sample();
        cfg.libkrun.path = "libkrun.dylib".into();
        assert_invalid(&cfg, "libkrun.path");
    }

    #[test]
    fn resources() {
        let mut cfg = sample();
        cfg.vcpus = 0;
        assert_invalid(&cfg, "vcpus");
        let mut cfg = sample();
        cfg.vcpus = u8::MAX;
        cfg.validate().unwrap();
        // 256 vCPUs does not even deserialize.
        let mut v = serde_json::to_value(sample()).unwrap();
        v["vcpus"] = 256.into();
        assert!(serde_json::from_value::<HelperConfig>(v).is_err());

        for mib in [0, MIN_MEMORY_MIB - 1, MAX_MEMORY_MIB + 1] {
            let mut cfg = sample();
            cfg.memory_mib = mib;
            assert_invalid(&cfg, "memory_mib");
        }
        let mut cfg = sample();
        cfg.log_level = MAX_LOG_LEVEL + 1;
        assert_invalid(&cfg, "log_level");
    }

    #[test]
    fn pins() {
        let bads = [
            SHA_A[..63].to_string(),
            SHA_A.to_uppercase(),
            format!("{}g", &SHA_A[..63]),
            String::new(),
        ];
        for bad in bads {
            let mut cfg = sample();
            cfg.libkrunfw.sha256.clone_from(&bad);
            assert_invalid(&cfg, "libkrunfw.sha256");
            let mut cfg = sample();
            cfg.root.sha256 = Some(bad);
            assert_invalid(&cfg, "root.sha256");
        }
    }

    #[test]
    fn disks() {
        let mut cfg = sample();
        cfg.root.read_only = false;
        assert_invalid(&cfg, "root");
        let mut cfg = sample();
        cfg.root.block_id = "vda".into();
        assert_invalid(&cfg, "root");
        let mut cfg = sample();
        cfg.scratch.read_only = true;
        assert_invalid(&cfg, "scratch");

        let ws = |id: &str, path: &str| DiskSpec {
            block_id: id.into(),
            path: path.into(),
            read_only: false,
            sha256: None,
            stamp: None,
        };
        let mut cfg = sample();
        cfg.disks.push(ws("ws-0", "/v/ws0.ext4"));
        cfg.validate().unwrap();
        cfg.disks.push(ws(SCRATCH_DRIVE_ID, "/v/ws1.ext4"));
        assert_invalid(&cfg, "disks");
        let mut cfg = sample();
        cfg.disks
            .push(ws("ws-1", "/h/.sandcrab/run/krun/id/scratch.ext4"));
        assert_invalid(&cfg, "disks");
        let mut cfg = sample();
        cfg.disks
            .push(ws(&"x".repeat(MAX_BLOCK_ID + 1), "/v/a.ext4"));
        assert_invalid(&cfg, "disks[0].block_id");
        let mut cfg = sample();
        cfg.disks = (0..=MAX_EXTRA_DISKS)
            .map(|i| ws(&format!("ws{i}"), &format!("/v/{i}.ext4")))
            .collect();
        assert_invalid(&cfg, "disks");
    }

    #[test]
    fn virtiofs_tags() {
        let mut cfg = sample();
        cfg.virtiofs[1].tag = "ws0".into();
        assert_invalid(&cfg, "virtiofs");
        let too_long = "t".repeat(MAX_VIRTIOFS_TAG + 1);
        for bad in ["", "has space", "a/b", "ünï", too_long.as_str()] {
            let mut cfg = sample();
            cfg.virtiofs[0].tag = bad.into();
            assert_invalid(&cfg, "virtiofs[0].tag");
        }
        let mut cfg = sample();
        cfg.virtiofs[0].tag = "t".repeat(MAX_VIRTIOFS_TAG);
        cfg.validate().unwrap();
        let mut cfg = sample();
        cfg.virtiofs = (0..=MAX_VIRTIOFS_SHARES)
            .map(|i| VirtioFsShare {
                tag: format!("ws{i}"),
                host_path: format!("/w/{i}").into(),
                read_only: true,
                identity: None,
            })
            .collect();
        assert_invalid(&cfg, "virtiofs");
    }

    #[test]
    fn read_from_leaves_the_lifeline_untouched() {
        let mut stream = sample().to_line().unwrap();
        stream.extend_from_slice(b"rest");
        let mut cur = Cursor::new(stream);
        HelperConfig::read_from(&mut cur).unwrap();
        let mut rest = String::new();
        cur.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "rest");
    }

    #[test]
    fn read_from_limits_and_errors() {
        let huge = vec![b' '; MAX_HELPER_CONFIG_BYTES + 10];
        assert!(matches!(
            HelperConfig::read_from(Cursor::new(huge)),
            Err(HelperConfigError::TooLarge { .. })
        ));
        // Backend died mid-write: truncated JSON, then EOF.
        let line = sample().to_line().unwrap();
        assert!(matches!(
            HelperConfig::read_from(Cursor::new(&line[..line.len() / 2])),
            Err(HelperConfigError::Json(_))
        ));
        assert!(matches!(
            HelperConfig::read_from(Cursor::new(b"")),
            Err(HelperConfigError::Json(_))
        ));
        // Parsed but invalid: validation runs on read too.
        let mut v = serde_json::to_value(sample()).unwrap();
        v["vcpus"] = 0.into();
        assert!(matches!(
            HelperConfig::read_from(Cursor::new(format!("{v}\n"))),
            Err(HelperConfigError::Invalid { .. })
        ));
    }

    #[test]
    fn to_line_refuses_invalid_configs() {
        let mut cfg = sample();
        cfg.exec.args.push("x\ny".into());
        assert!(cfg.to_line().is_err());
    }

    #[test]
    fn exit_codes() {
        assert_eq!(
            exit_code::describe(71),
            Some("pinned artifact SHA-256 mismatch")
        );
        assert_eq!(exit_code::describe(126), Some("guest init failed"));
        assert_eq!(exit_code::describe(0), None);
        assert_eq!(exit_code::describe(1), None);
    }
}
