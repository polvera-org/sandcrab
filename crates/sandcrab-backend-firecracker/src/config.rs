//! [`FirecrackerConfig`]: every path and knob of the backend, explicit.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sandcrab_core::archive::UnpackLimits;
use sandcrab_core::{Result, SandboxError, UserSpec};

use crate::artifacts::{FIRECRACKER_X86_64, KERNEL_X86_64};
use crate::net::TapPoolConfig;
use crate::rootfs::MkfsStrategy;

/// Where the guest root filesystem comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootfsSource {
    /// Build (once, cached by image id + crabd sha) from the template's OCI image with
    /// [`crate::rootfs::RootfsBuilder`], injecting this static musl `crabd`. Needs Docker for
    /// the export on a cache miss and for the image-id/pin check.
    Template { crabd: PathBuf },
    /// A prebuilt ext4 image used for every template (custom pipelines, tests).
    Image(PathBuf),
}

/// Firecracker backend configuration. Build one with [`FirecrackerConfig::with_home`] /
/// [`FirecrackerConfig::from_env`] and adjust fields.
#[derive(Debug, Clone)]
pub struct FirecrackerConfig {
    /// `firecracker` binary (pinned v1.18.0 under the artifact cache by default).
    pub firecracker_bin: PathBuf,
    /// Uncompressed guest kernel (`vmlinux`, pinned 6.18).
    pub kernel: PathBuf,
    pub rootfs: RootfsSource,
    /// Artifact / rootfs / blank-scratch / read-only-workspace image cache.
    pub cache_root: PathBuf,
    /// Per-sandbox run dirs `<run_dir>/<sandbox id>/` (0700). Tap locks are host-wide, not
    /// here (see [`TapPoolConfig::lock_dir`]).
    /// Kept apart from Docker's `run/` so neither backend's orphan reaper sees the other's
    /// directories. Socket paths must stay ≤ 108 bytes, so keep this short.
    pub run_dir: PathBuf,
    /// Images of rw workspaces whose sync-back failed are moved here
    /// (`<recovery_dir>/<sandbox id>/ws-<n>.ext4` + `info.json`).
    pub recovery_dir: PathBuf,
    /// `mke2fs` strategy for workspace images; `None` = auto-detect once at construction.
    pub mkfs: Option<MkfsStrategy>,
    /// Check the kernel and `firecracker` against the pinned SHA-256s at construction.
    pub verify_pins: bool,
    /// Tap pool used for `unrestricted` networking (see `scripts/fc-net-setup.sh`).
    pub taps: TapPoolConfig,
    /// Guest nameservers when networking is enabled.
    pub dns: Vec<IpAddr>,
    /// Delegated cgroup v2 directory; each VMM gets a `<id>` leaf with `memory.max` /
    /// `pids.max`. `None` = no cgroup (limits only through `/machine-config`).
    pub cgroup_parent: Option<PathBuf>,
    /// VMM memory headroom added to the guest RAM for `memory.max`.
    pub vmm_overhead_mib: u64,
    /// `pids.max` of the VMM leaf cgroup.
    pub pids_max: u32,
    /// Deadline from VMM launch to the guest control port answering (and `init.configure`).
    pub boot_timeout: Duration,
    /// Per-request Firecracker API timeout.
    pub api_timeout: Duration,
    /// Grace between SIGTERM and SIGKILL of guest processes on `init.shutdown`.
    pub shutdown_grace_ms: u64,
    /// If the VMM has not exited this long after teardown starts its shutdown step, send
    /// `SendCtrlAltDel`.
    pub ctrl_alt_del_after: Duration,
    /// ... and SIGKILL it at this point.
    pub kill_after: Duration,
    /// Idle timeout while reading a workspace export stream.
    pub export_idle_timeout: Duration,
    /// Overall export deadline per drive: `export_timeout + drive size /
    /// export_min_bytes_per_sec`. When it passes the stream is aborted, the guest image is
    /// preserved under [`Self::recovery_dir`], and teardown proceeds (the VM is killed).
    pub export_timeout: Duration,
    /// See [`Self::export_timeout`].
    pub export_min_bytes_per_sec: u64,
    /// Upper caps for the (hostile) export archive. Each sync-back further bounds them by the
    /// workspace drive's size ([`UnpackLimits::bounded_by`]) and [`Self::sync_back_quota`].
    pub unpack_limits: UnpackLimits,
    /// Optional host-side cap on the file content one workspace sync-back may write.
    pub sync_back_quota: Option<u64>,
    /// Re-enable the serial console (drops `8250.nr_uarts=0 quiet`); output goes to
    /// `firecracker.log`.
    pub debug_console: bool,
    /// `firecracker --level`.
    pub log_level: String,
    /// Guest user when neither the sandbox config nor the template sets one; `None` = the
    /// host uid:gid (same default as Docker).
    pub default_user: Option<UserSpec>,
}

impl FirecrackerConfig {
    /// Defaults rooted at a sandcrab home (`~/.sandcrab`): artifacts from the pinned cache
    /// layout, rootfs built from the template image with `crabd`.
    #[must_use]
    pub fn with_home(home: &Path, crabd: impl Into<PathBuf>) -> Self {
        let cache_root = home.join("cache");
        Self {
            firecracker_bin: cache_root
                .join("firecracker")
                .join(FIRECRACKER_X86_64.version)
                .join("firecracker"),
            kernel: cache_root
                .join("kernel")
                .join(KERNEL_X86_64.version)
                .join("vmlinux"),
            rootfs: RootfsSource::Template {
                crabd: crabd.into(),
            },
            run_dir: home.join("run").join("fc"),
            recovery_dir: home.join("recovery"),
            cache_root,
            mkfs: None,
            verify_pins: true,
            taps: TapPoolConfig::default(),
            dns: vec![
                IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
                IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
            ],
            cgroup_parent: None,
            vmm_overhead_mib: 64,
            pids_max: 64,
            boot_timeout: Duration::from_secs(10),
            api_timeout: crate::api::DEFAULT_API_TIMEOUT,
            shutdown_grace_ms: sandcrab_protocol::init::DEFAULT_SHUTDOWN_GRACE_MS,
            ctrl_alt_del_after: Duration::from_millis(1500),
            kill_after: Duration::from_secs(3),
            export_idle_timeout: crate::workspace::EXPORT_IDLE_TIMEOUT,
            export_timeout: crate::workspace::EXPORT_TIMEOUT,
            export_min_bytes_per_sec: crate::workspace::EXPORT_MIN_BYTES_PER_SEC,
            unpack_limits: UnpackLimits::default(),
            sync_back_quota: None,
            debug_console: false,
            log_level: "Warn".into(),
            default_user: None,
        }
    }

    /// [`Self::with_home`] at `$SANDCRAB_HOME` / `~/.sandcrab`.
    pub fn from_env(crabd: impl Into<PathBuf>) -> Result<Self> {
        let home =
            crate::sandcrab_home().map_err(|e| SandboxError::InvalidConfig(e.to_string()))?;
        Ok(Self::with_home(&home, crabd))
    }

    /// Download (if missing) and verify the pinned kernel and `firecracker` into
    /// `cache_root`, and point the config at them. Blocking (uses `curl`).
    pub fn fetch_pinned_artifacts(&mut self) -> Result<()> {
        let to_err = |e: crate::Error| SandboxError::Backend {
            backend: "firecracker".into(),
            message: format!("fetching pinned artifacts: {e}"),
        };
        self.kernel = crate::artifacts::fetch_kernel(&self.cache_root).map_err(to_err)?;
        self.firecracker_bin = crate::artifacts::fetch_firecracker(&self.cache_root, false)
            .map_err(to_err)?
            .firecracker;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        self.taps.validate()?;
        if self.ctrl_alt_del_after >= self.kill_after {
            return Err(SandboxError::InvalidConfig(
                "ctrl_alt_del_after must be shorter than kill_after".into(),
            ));
        }
        if self.boot_timeout.is_zero() || self.api_timeout.is_zero() {
            return Err(SandboxError::InvalidConfig(
                "boot_timeout and api_timeout must be positive".into(),
            ));
        }
        if self.export_idle_timeout.is_zero() || self.export_timeout.is_zero() {
            return Err(SandboxError::InvalidConfig(
                "export_idle_timeout and export_timeout must be positive".into(),
            ));
        }
        // <run_dir>/<36-char uuid>/api.sock must fit sun_path (108 incl. NUL).
        let longest = self.run_dir.as_os_str().len() + 1 + 36 + 1 + "api.sock".len();
        if longest > 107 {
            return Err(SandboxError::InvalidConfig(format!(
                "run_dir `{}` is too long for Unix socket paths ({longest} > 107 bytes)",
                self.run_dir.display()
            )));
        }
        Ok(())
    }

    /// Kernel command line (fixed, not user-extensible; design §1).
    #[must_use]
    pub fn boot_args(&self, ip_arg: Option<&str>) -> String {
        let mut args = String::from("console=ttyS0 ");
        if !self.debug_console {
            args.push_str("8250.nr_uarts=0 quiet ");
        }
        args.push_str("reboot=k panic=1 pci=off ro root=/dev/vda rootfstype=ext4 init=/sbin/crabd");
        if let Some(ip) = ip_arg {
            args.push(' ');
            args.push_str(ip);
        }
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_validation() {
        let c = FirecrackerConfig::with_home(Path::new("/home/u/.sandcrab"), "/bin/crabd");
        c.validate().unwrap();
        assert!(c.kernel.ends_with("kernel/6.18.62/vmlinux"));
        assert!(
            c.firecracker_bin
                .ends_with("firecracker/1.18.0/firecracker")
        );
        assert_eq!(c.run_dir, Path::new("/home/u/.sandcrab/run/fc"));

        let mut long = c.clone();
        long.run_dir = PathBuf::from("/".repeat(80));
        assert!(long.validate().is_err());
        let mut bad = c.clone();
        bad.kill_after = Duration::from_secs(1);
        assert!(bad.validate().is_err());
    }

    #[test]
    fn boot_args() {
        let mut c = FirecrackerConfig::with_home(Path::new("/h"), "/c");
        assert_eq!(
            c.boot_args(None),
            "console=ttyS0 8250.nr_uarts=0 quiet reboot=k panic=1 pci=off ro root=/dev/vda \
             rootfstype=ext4 init=/sbin/crabd"
        );
        c.debug_console = true;
        let a = c.boot_args(Some("ip=172.30.0.2::172.30.0.1:255.255.255.252::eth0:off"));
        assert!(!a.contains("nr_uarts") && a.ends_with("eth0:off"));
    }
}
