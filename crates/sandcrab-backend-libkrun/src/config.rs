//! [`LibkrunConfig`]: every path and knob of the libkrun backend, explicit (design §15).

use std::path::{Path, PathBuf};
use std::time::Duration;

use sandcrab_core::{Result, SandboxError, UserSpec};

use crate::helper_proto::{MAX_LOG_LEVEL, MAX_SOCKET_PATH, VirtiofsSemantics};

/// File name of the helper binary (`src/bin/sandcrab-krun.rs`).
pub const HELPER_BIN_NAME: &str = "sandcrab-krun";

/// `$SANDCRAB_LIBKRUN_CGROUP`: delegated cgroup v2 parent for the helper leaves (Linux), read
/// by [`LibkrunConfig::from_env`] and the CLI.
pub const CGROUP_ENV: &str = "SANDCRAB_LIBKRUN_CGROUP";
/// The helper's entitlements (`entitlements.plist`: only `com.apple.security.hypervisor`),
/// embedded so installers can sign the helper without a source checkout (design §4).
pub const ENTITLEMENTS_PLIST: &str = include_str!("../entitlements.plist");
/// Entitlements for a hardened-runtime signature (Developer ID, review R10):
/// `entitlements-hardened.plist` adds `com.apple.security.cs.disable-library-validation`,
/// without which the hardened runtime refuses to load libkrun/libkrunfw (signed by another
/// team, or ad hoc). The helper verifies both libraries itself (pinned SHA-256, loaded through
/// the verified fd), so library validation adds nothing it relies on.
pub const ENTITLEMENTS_HARDENED_PLIST: &str = include_str!("../entitlements-hardened.plist");
/// `$SANDCRAB_SIGN_RUNTIME=1`: sign with the hardened runtime even ad hoc (tests the Developer
/// ID configuration locally; a Developer ID identity always gets it).
pub const SIGN_RUNTIME_ENV: &str = "SANDCRAB_SIGN_RUNTIME";
/// The entitlement the helper needs on macOS to create VMs.
pub const HYPERVISOR_ENTITLEMENT: &str = "com.apple.security.hypervisor";
/// Control socket name inside a sandbox run dir (guest vsock port 1).
pub const CONTROL_SOCKET_NAME: &str = "ctl.sock";
/// Tools socket name inside a sandbox run dir (guest vsock port 1024).
pub const TOOLS_SOCKET_NAME: &str = "v.sock";
/// Length of a sandbox id (hyphenated UUID), the run dir component under [`LibkrunConfig::run_dir`].
const SANDBOX_ID_LEN: usize = 36;

/// Where the guest root filesystem comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootfsSource {
    /// Build (once, cached by image id + arch + crabd sha) from the template's OCI image with
    /// the shared `sandcrab-rootfs` pipeline, injecting this static musl `crabd` for the
    /// guest arch. Needs Docker on a cache miss (build time only).
    Template { crabd: PathBuf },
    /// A prebuilt ext4 image used for every template (custom pipelines, tests).
    Image(PathBuf),
}

/// libkrun backend configuration. Build one with [`LibkrunConfig::with_home`] /
/// [`LibkrunConfig::from_env`] and adjust fields.
#[derive(Debug, Clone)]
pub struct LibkrunConfig {
    /// The signed `sandcrab-krun` helper (needs `com.apple.security.hypervisor` on macOS).
    pub helper: PathBuf,
    /// Directory holding the pinned libkrun + libkrunfw. `None` = the pinned artifact dir
    /// (`~/.sandcrab/artifacts/libkrun/<ver>/`); never a system or Homebrew path implicitly.
    pub lib_dir: Option<PathBuf>,
    /// Directory holding the pinned userspace netstack install (passt on Linux, gvproxy on
    /// macOS; `sandcrab libkrun install`) for network `unrestricted`. `None` = no netstack:
    /// that policy is then refused. Default: `<home>/artifacts/libkrun-net/<kind>-<version>/`.
    pub netstack_dir: Option<PathBuf>,
    pub rootfs: RootfsSource,
    /// Rootfs / blank-scratch image cache.
    pub cache_root: PathBuf,
    /// Per-sandbox run dirs `<run_dir>/<sandbox id>/` (0700) with `ctl.sock`, `v.sock`,
    /// `vmm.log`, `meta.json` and the scratch image. Kept apart from the Docker and
    /// Firecracker run dirs so no reaper sees another backend's directories. Socket paths
    /// must fit [`MAX_SOCKET_PATH`] bytes, so keep this short.
    pub run_dir: PathBuf,
    /// Check libkrun, libkrunfw and the rootfs against their pinned SHA-256s.
    pub verify_pins: bool,
    /// Deadline from helper launch to the guest control port answering (and `init.configure`).
    pub boot_timeout: Duration,
    /// Grace between SIGTERM and SIGKILL of guest processes on `init.shutdown`.
    pub shutdown_grace_ms: u64,
    /// If the helper has not exited this long after teardown starts, SIGTERM its process group…
    pub term_after: Duration,
    /// … and SIGKILL it at this point.
    pub kill_after: Duration,
    /// Deprecated. Allow `network.policy = unrestricted` through TSI when the pinned userspace
    /// netstack is **not** installed. **The guest can reach host loopback services.** Ignored
    /// (with an error log) when the netstack is installed; off by default (design §9).
    pub allow_tsi_unrestricted: bool,
    /// Ownership semantics of virtio-fs workspace shares.
    pub virtiofs_semantics: VirtiofsSemantics,
    /// libkrun log level (0 = off … 5 = trace), written to `vmm.log`.
    pub log_level: u32,
    /// Guest user when neither the sandbox config nor the template sets one; `None` = the
    /// host uid:gid (same default as Docker, matches virtio-fs ownership).
    pub default_user: Option<UserSpec>,
    /// Linux only: delegated cgroup v2 directory; each helper gets a `<id>` leaf with
    /// `memory.max` / `pids.max`. `None` = no cgroup. Ignored on macOS (no cgroups).
    pub cgroup_parent: Option<PathBuf>,
    /// VMM memory headroom added to the guest RAM for `memory.max` (Linux).
    pub vmm_overhead_mib: u64,
    /// `pids.max` of the helper leaf cgroup (Linux).
    pub pids_max: u32,
}

impl LibkrunConfig {
    /// Defaults rooted at a sandcrab home (`~/.sandcrab`): helper under `bin/`, libraries from
    /// the pinned artifact dir, rootfs built from the template image with `crabd`.
    #[must_use]
    pub fn with_home(home: &Path, crabd: impl Into<PathBuf>) -> Self {
        Self {
            helper: home.join("bin").join(HELPER_BIN_NAME),
            lib_dir: None,
            netstack_dir: crate::netstack_pins::host_pin()
                .ok()
                .map(|pin| crate::netstack_pins::default_dir(home, pin)),
            rootfs: RootfsSource::Template {
                crabd: crabd.into(),
            },
            cache_root: home.join("cache"),
            run_dir: home.join("run").join("krun"),
            verify_pins: true,
            boot_timeout: Duration::from_secs(10),
            shutdown_grace_ms: sandcrab_protocol::init::DEFAULT_SHUTDOWN_GRACE_MS,
            term_after: Duration::from_millis(1500),
            kill_after: Duration::from_secs(3),
            allow_tsi_unrestricted: false,
            virtiofs_semantics: VirtiofsSemantics::default(),
            log_level: 0,
            default_user: None,
            cgroup_parent: None,
            vmm_overhead_mib: 128,
            pids_max: 64,
        }
    }

    /// [`Self::with_home`] at `$SANDCRAB_HOME` / `~/.sandcrab`, with `cgroup_parent` from
    /// `$SANDCRAB_LIBKRUN_CGROUP` ([`CGROUP_ENV`]) when set.
    pub fn from_env(crabd: impl Into<PathBuf>) -> Result<Self> {
        let home = sandcrab_rootfs::sandcrab_home()
            .map_err(|e| SandboxError::InvalidConfig(e.to_string()))?;
        let mut c = Self::with_home(&home, crabd);
        c.cgroup_parent = cgroup_parent_from_env();
        Ok(c)
    }

    /// Run dir of one sandbox.
    #[must_use]
    pub fn sandbox_run_dir(&self, sandbox_id: &str) -> PathBuf {
        self.run_dir.join(sandbox_id)
    }

    pub fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(SandboxError::InvalidConfig(m));
        if self.helper.as_os_str().is_empty() {
            return bad("libkrun helper path is empty".into());
        }
        if let Some(dir) = &self.lib_dir
            && !dir.is_absolute()
        {
            return bad(format!(
                "libkrun lib_dir `{}` must be absolute",
                dir.display()
            ));
        }
        if self.boot_timeout.is_zero() {
            return bad("boot_timeout must be positive".into());
        }
        if self.term_after >= self.kill_after {
            return bad("term_after must be shorter than kill_after".into());
        }
        if self.log_level > MAX_LOG_LEVEL {
            return bad(format!("log_level must be ≤ {MAX_LOG_LEVEL}"));
        }
        // <run_dir>/<36-char id>/ctl.sock must fit the macOS sun_path; never truncated.
        let socket = CONTROL_SOCKET_NAME.len().max(TOOLS_SOCKET_NAME.len());
        let longest = self.run_dir.as_os_str().len() + 1 + SANDBOX_ID_LEN + 1 + socket;
        if longest > MAX_SOCKET_PATH {
            return bad(format!(
                "run_dir `{}` is too long for Unix socket paths ({longest} > {MAX_SOCKET_PATH} \
                 bytes); shorten SANDCRAB_HOME",
                self.run_dir.display()
            ));
        }
        Ok(())
    }
}

/// `$SANDCRAB_LIBKRUN_CGROUP` as a path, if set and non-empty.
#[must_use]
pub fn cgroup_parent_from_env() -> Option<PathBuf> {
    std::env::var_os(CGROUP_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_validation() {
        let c = LibkrunConfig::with_home(Path::new("/Users/u/.sandcrab"), "/bin/crabd");
        c.validate().unwrap();
        assert_eq!(c.helper, Path::new("/Users/u/.sandcrab/bin/sandcrab-krun"));
        assert_eq!(c.run_dir, Path::new("/Users/u/.sandcrab/run/krun"));
        assert_eq!(c.cache_root, Path::new("/Users/u/.sandcrab/cache"));
        assert!(c.lib_dir.is_none());
        assert!(!c.allow_tsi_unrestricted, "TSI is never on by default");
        assert_eq!(c.virtiofs_semantics, VirtiofsSemantics::LinuxComplete);
        assert_eq!(
            c.rootfs,
            RootfsSource::Template {
                crabd: "/bin/crabd".into()
            }
        );
        assert_eq!(
            c.sandbox_run_dir("abc"),
            Path::new("/Users/u/.sandcrab/run/krun/abc")
        );
    }

    #[test]
    fn socket_budget() {
        let mut c = LibkrunConfig::with_home(Path::new("/h"), "/c");
        // 103 - (1 + 36 + 1 + "ctl.sock") = 57 bytes of run_dir fit exactly.
        c.run_dir = PathBuf::from(format!("/{}", "r".repeat(56)));
        c.validate().unwrap();
        let id = "00000000-0000-0000-0000-000000000000";
        let sock = c.sandbox_run_dir(id).join(CONTROL_SOCKET_NAME);
        assert_eq!(sock.as_os_str().len(), MAX_SOCKET_PATH);
        c.run_dir = PathBuf::from(format!("/{}", "r".repeat(57)));
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("SANDCRAB_HOME"), "{err}");
    }

    #[test]
    fn invalid_knobs() {
        let base = LibkrunConfig::with_home(Path::new("/h"), "/c");
        let mut c = base.clone();
        c.kill_after = Duration::from_secs(1);
        assert!(c.validate().is_err());
        let mut c = base.clone();
        c.boot_timeout = Duration::ZERO;
        assert!(c.validate().is_err());
        let mut c = base.clone();
        c.lib_dir = Some("rel/lib".into());
        assert!(c.validate().is_err());
        let mut c = base.clone();
        c.log_level = 6;
        assert!(c.validate().is_err());
        let mut c = base;
        c.helper = PathBuf::new();
        assert!(c.validate().is_err());
    }
}
