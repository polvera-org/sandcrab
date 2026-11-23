//! `unrestricted` networking: a pre-created, user-owned tap pool (design §6).
//!
//! `scripts/fc-net-setup.sh` (root, once) creates `sctap0..sctap<N-1>` owned by the sandcrab
//! user, gives tap `n` the host address `172.30.<n>.1/30`, and installs a dedicated
//! `inet sandcrab` nftables table (masquerade out the uplink, no guest→host, no
//! guest→guest). At runtime the backend needs **no privileges**: it takes an exclusive
//! `flock` on `<run_root>/net/<tap>.lock`, hands the tap name to Firecracker (a user-owned
//! tap can be attached with an unprivileged `TUNSETIFF`), and configures the guest with the
//! kernel `ip=` parameter. The lock is released when the [`TapLease`] drops (or the process
//! dies), so a crashed sandcrab never leaks a tap.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use sandcrab_core::{Result, SandboxError};

/// Network address plan of tap `index` (must match `scripts/fc-net-setup.sh`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapPlan {
    pub index: u32,
    pub name: String,
    pub host_ip: Ipv4Addr,
    pub guest_ip: Ipv4Addr,
    pub guest_mac: String,
}

impl TapPlan {
    /// `index` must be < 256 (one /30 per third octet of `172.30.0.0/16`).
    #[must_use]
    pub fn new(prefix: &str, index: u32) -> Self {
        let n = u8::try_from(index).expect("tap index < 256");
        Self {
            index,
            name: format!("{prefix}{index}"),
            host_ip: Ipv4Addr::new(172, 30, n, 1),
            guest_ip: Ipv4Addr::new(172, 30, n, 2),
            guest_mac: format!("06:00:AC:1E:{n:02X}:02"),
        }
    }

    /// Kernel `ip=` argument (`IP_PNP`): `<client>::<gw>:<netmask>::<dev>:<autoconf>`.
    #[must_use]
    pub fn kernel_ip_arg(&self) -> String {
        format!(
            "ip={}::{}:255.255.255.252::eth0:off",
            self.guest_ip, self.host_ip
        )
    }
}

/// Tap pool settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapPoolConfig {
    /// Tap name prefix (`sctap` → `sctap0`, `sctap1`, ...). Linux names are ≤ 15 bytes.
    pub prefix: String,
    /// Number of taps the setup script created (1..=256).
    pub size: u32,
    /// Where interfaces are listed (`/sys/class/net`; overridable for tests).
    pub sysfs_net: PathBuf,
}

impl Default for TapPoolConfig {
    fn default() -> Self {
        Self {
            prefix: "sctap".into(),
            size: 64,
            sysfs_net: PathBuf::from("/sys/class/net"),
        }
    }
}

impl TapPoolConfig {
    pub fn validate(&self) -> Result<()> {
        let max_name = format!("{}{}", self.prefix, self.size.saturating_sub(1));
        if self.prefix.is_empty()
            || max_name.len() > 15
            || !self
                .prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(SandboxError::InvalidConfig(format!(
                "tap prefix `{}` must be [A-Za-z0-9_-]+ and leave tap names ≤ 15 bytes",
                self.prefix
            )));
        }
        if self.size == 0 || self.size > 256 {
            return Err(SandboxError::InvalidConfig(format!(
                "tap pool size {} must be 1..=256",
                self.size
            )));
        }
        Ok(())
    }
}

/// An exclusively held tap; released on drop.
#[derive(Debug)]
pub struct TapLease {
    pub plan: TapPlan,
    lock_path: PathBuf,
    _lock: File,
}

impl TapLease {
    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

fn setup_hint(cfg: &TapPoolConfig) -> String {
    format!(
        "run `sudo scripts/fc-net-setup.sh --user $USER --count {}` once on this host \
         (creates {}0..{}{} and the `inet sandcrab` nftables table)",
        cfg.size,
        cfg.prefix,
        cfg.prefix,
        cfg.size - 1
    )
}

/// Owner uid of a tap (`/sys/class/net/<tap>/owner`; `-1` = unowned).
fn tap_owner(sysfs_net: &Path, name: &str) -> Option<i64> {
    fs::read_to_string(sysfs_net.join(name).join("owner"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Take the first free tap of the pool owned by `uid`.
pub fn acquire_tap(cfg: &TapPoolConfig, lock_dir: &Path, uid: u32) -> Result<TapLease> {
    cfg.validate()?;
    fs::create_dir_all(lock_dir).map_err(|e| io_at(lock_dir, e))?;
    let mut present = 0u32;
    let mut foreign = Vec::new();
    for index in 0..cfg.size {
        let plan = TapPlan::new(&cfg.prefix, index);
        if !cfg.sysfs_net.join(&plan.name).exists() {
            continue;
        }
        present += 1;
        match tap_owner(&cfg.sysfs_net, &plan.name) {
            Some(owner) if owner == i64::from(uid) => {}
            _ => {
                foreign.push(plan.name);
                continue;
            }
        }
        let lock_path = lock_dir.join(format!("{}.lock", plan.name));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| io_at(&lock_path, e))?;
        match file.try_lock() {
            Ok(()) => {
                return Ok(TapLease {
                    plan,
                    lock_path,
                    _lock: file,
                });
            }
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(e)) => return Err(io_at(&lock_path, e)),
        }
    }
    let why = if present == 0 {
        format!("no `{}*` tap devices exist", cfg.prefix)
    } else if foreign.len() == present as usize {
        format!(
            "none of the {present} tap(s) is owned by uid {uid} (e.g. {})",
            foreign[0]
        )
    } else {
        format!("all usable taps ({present} present) are in use")
    };
    Err(SandboxError::Backend {
        backend: "firecracker".into(),
        message: format!(
            "network policy `unrestricted` needs a free tap: {why}; {}",
            setup_hint(cfg)
        ),
    })
}

fn io_at(path: &Path, e: io::Error) -> SandboxError {
    SandboxError::Io(io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_matches_setup_script() {
        let p = TapPlan::new("sctap", 0);
        assert_eq!(p.name, "sctap0");
        assert_eq!(p.host_ip, Ipv4Addr::new(172, 30, 0, 1));
        assert_eq!(p.guest_ip, Ipv4Addr::new(172, 30, 0, 2));
        assert_eq!(p.guest_mac, "06:00:AC:1E:00:02");
        let p = TapPlan::new("sctap", 255);
        assert_eq!(p.guest_mac, "06:00:AC:1E:FF:02");
        assert_eq!(
            p.kernel_ip_arg(),
            "ip=172.30.255.2::172.30.255.1:255.255.255.252::eth0:off"
        );
    }

    #[test]
    fn pool_validation() {
        TapPoolConfig::default().validate().unwrap();
        for bad in [
            TapPoolConfig {
                size: 0,
                ..Default::default()
            },
            TapPoolConfig {
                size: 257,
                ..Default::default()
            },
            TapPoolConfig {
                prefix: "a-very-long-prefix".into(),
                ..Default::default()
            },
            TapPoolConfig {
                prefix: "bad/name".into(),
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    fn fake_tap(sysfs: &Path, name: &str, owner: &str) {
        fs::create_dir_all(sysfs.join(name)).unwrap();
        fs::write(sysfs.join(name).join("owner"), format!("{owner}\n")).unwrap();
    }

    #[test]
    fn acquires_free_owned_taps_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let sysfs = dir.path().join("net");
        let locks = dir.path().join("locks");
        let cfg = TapPoolConfig {
            prefix: "sctap".into(),
            size: 4,
            sysfs_net: sysfs.clone(),
        };

        let err = acquire_tap(&cfg, &locks, 1000).unwrap_err().to_string();
        assert!(err.contains("no `sctap*` tap devices exist"), "{err}");
        assert!(err.contains("fc-net-setup.sh"), "{err}");

        fake_tap(&sysfs, "sctap0", "0");
        let err = acquire_tap(&cfg, &locks, 1000).unwrap_err().to_string();
        assert!(err.contains("owned by uid 1000"), "{err}");

        fake_tap(&sysfs, "sctap1", "1000");
        fake_tap(&sysfs, "sctap3", "1000");
        let a = acquire_tap(&cfg, &locks, 1000).unwrap();
        assert_eq!(a.plan.name, "sctap1");
        let b = acquire_tap(&cfg, &locks, 1000).unwrap();
        assert_eq!(b.plan.index, 3);
        let err = acquire_tap(&cfg, &locks, 1000).unwrap_err().to_string();
        assert!(err.contains("in use"), "{err}");
        drop(a);
        let c = acquire_tap(&cfg, &locks, 1000).unwrap();
        assert_eq!(c.plan.name, "sctap1");
        assert!(c.lock_path().ends_with("sctap1.lock"));
    }
}
