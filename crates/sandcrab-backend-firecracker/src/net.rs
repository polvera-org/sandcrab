//! `unrestricted` networking: a pre-created, user-owned tap pool (design §6).
//!
//! `scripts/fc-net-setup.sh` (root, once) creates `sctap0..sctap<N-1>` owned by the sandcrab
//! user, gives tap `n` the host address `172.30.<n>.1/30`, and installs a dedicated
//! `inet sandcrab` nftables table (masquerade out the uplink, no guest→host, no
//! guest→guest). At runtime the backend needs **no privileges**: it takes an exclusive
//! `flock` on `<lock dir>/<tap>.lock`, hands the tap name to Firecracker (a user-owned
//! tap can be attached with an unprivileged `TUNSETIFF`), and configures the guest with the
//! kernel `ip=` parameter. The lock is released when the [`TapLease`] drops (or the process
//! dies), so a crashed sandcrab never leaks a tap.
//!
//! The taps are a host resource, so the lock dir is **host-wide** per user, not per sandcrab
//! home: every sandcrab process of this uid (any `SANDCRAB_HOME`, any test state dir) must
//! see the same locks or two of them hand out the same tap. See [`default_lock_dir`].

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::net::Ipv4Addr;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
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
    /// Directory of the `<tap>.lock` lease files. `None` = the host-wide per-user default
    /// ([`default_lock_dir`]). Every process sharing this tap pool must use the same dir.
    /// It must be owned by the current user with mode 0700, and its parent must not be
    /// writable by anyone else, else leasing fails.
    pub lock_dir: Option<PathBuf>,
}

impl Default for TapPoolConfig {
    fn default() -> Self {
        Self {
            prefix: "sctap".into(),
            size: 64,
            sysfs_net: PathBuf::from("/sys/class/net"),
            lock_dir: None,
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
///
/// The `flock` belongs to the lock file's open file description. The fd is `O_CLOEXEC`, but
/// a child forked by *another thread* of this process holds a copy of it until its `execve`
/// completes, so the release can lag a drop by that fork→exec window (sub-millisecond).
/// [`acquire_tap`] then just moves on to the next tap.
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

/// Host-wide tap lock dir of the current user, independent of any sandcrab home:
/// 1. `/run/user/<euid>/sandcrab/taps` when that runtime dir exists and is ours (the same
///    path for every session, so preferred over the env var);
/// 2. else `$XDG_RUNTIME_DIR/sandcrab/taps` when it is an absolute dir owned by us;
/// 3. else `$TMPDIR/sandcrab-<euid>/taps` (usually `/tmp`).
///
/// Created 0700 on first use and owner/mode-checked on every lease ([`acquire_tap`]).
#[must_use]
pub fn default_lock_dir() -> PathBuf {
    let euid = rustix::process::geteuid().as_raw();
    let owned_dir = |p: &PathBuf| {
        p.is_absolute() && fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && m.uid() == euid)
    };
    let runtime = Some(PathBuf::from(format!("/run/user/{euid}")))
        .filter(owned_dir)
        .or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(owned_dir)
        });
    match runtime {
        Some(dir) => dir.join("sandcrab").join("taps"),
        None => std::env::temp_dir()
            .join(format!("sandcrab-{euid}"))
            .join("taps"),
    }
}

/// Create `dir` (and missing parents) 0700, then check it before trusting its lease files:
/// - `dir` must be a real directory (not a symlink) owned by `euid` with mode exactly 0700;
/// - its parent must be a real directory owned by `euid` (or root) that no group/other can
///   write, so nobody else can rename `dir` away and plant their own.
///
/// So a pre-created dir in a shared location (e.g. `/tmp/sandcrab-<uid>` made by another user,
/// whose owner could swap or pre-lock the lease files) is refused rather than used.
fn ensure_private_lock_dir(dir: &Path, euid: u32) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| io_at(dir, e))?;
    let check = |d: &Path, is_parent: bool| -> Result<()> {
        let m = fs::symlink_metadata(d).map_err(|e| io_at(d, e))?;
        let mode = m.mode() & 0o7777;
        let (ok, want) = if is_parent {
            (
                m.is_dir() && (m.uid() == euid || m.uid() == 0) && mode & 0o022 == 0,
                format!("a directory owned by uid {euid} or root, not group/other-writable"),
            )
        } else {
            (
                m.is_dir() && m.uid() == euid && mode == 0o700,
                format!("a directory owned by uid {euid} with mode 0700"),
            )
        };
        if ok {
            return Ok(());
        }
        let kind = if m.is_symlink() {
            "a symlink"
        } else if m.is_dir() {
            "a directory"
        } else {
            "a non-directory"
        };
        Err(SandboxError::Backend {
            backend: "firecracker".into(),
            message: format!(
                "refusing tap lock dir {}: {} must be {want} (found {kind}, uid {}, mode \
                 {mode:o}); fix or remove it, or set `taps.lock_dir`",
                dir.display(),
                d.display(),
                m.uid(),
            ),
        })
    };
    check(dir, false)?;
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        check(parent, true)?;
    }
    Ok(())
}

/// Take the first free tap of the pool owned by `uid`, leasing it in `cfg.lock_dir`
/// (default: [`default_lock_dir`]).
pub fn acquire_tap(cfg: &TapPoolConfig, uid: u32) -> Result<TapLease> {
    cfg.validate()?;
    let lock_dir = cfg.lock_dir.clone().unwrap_or_else(default_lock_dir);
    let lock_dir = lock_dir.as_path();
    ensure_private_lock_dir(lock_dir, rustix::process::geteuid().as_raw())?;
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
        // O_NOFOLLOW: never lock through a symlink (std already sets O_CLOEXEC).
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
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
        let dir = private_tempdir();
        let sysfs = dir.path().join("net");
        let locks = dir.path().join("locks");
        let cfg = TapPoolConfig {
            prefix: "sctap".into(),
            size: 4,
            sysfs_net: sysfs.clone(),
            lock_dir: Some(locks),
        };

        let err = acquire_tap(&cfg, 1000).unwrap_err().to_string();
        assert!(err.contains("no `sctap*` tap devices exist"), "{err}");
        assert!(err.contains("fc-net-setup.sh"), "{err}");

        fake_tap(&sysfs, "sctap0", "0");
        let err = acquire_tap(&cfg, 1000).unwrap_err().to_string();
        assert!(err.contains("owned by uid 1000"), "{err}");

        fake_tap(&sysfs, "sctap1", "1000");
        fake_tap(&sysfs, "sctap3", "1000");
        let a = acquire_tap(&cfg, 1000).unwrap();
        assert_eq!(a.plan.name, "sctap1");
        let b = acquire_tap(&cfg, 1000).unwrap();
        assert_eq!(b.plan.index, 3);
        let err = acquire_tap(&cfg, 1000).unwrap_err().to_string();
        assert!(err.contains("in use"), "{err}");
        drop(a);
        // Not necessarily immediate: a process spawned concurrently by another test can hold
        // the lock fd for its fork→exec window (see `TapLease`), so retry briefly.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let c = loop {
            match acquire_tap(&cfg, 1000) {
                Ok(c) => break c,
                Err(e) if std::time::Instant::now() < deadline => {
                    assert!(e.to_string().contains("in use"), "{e}");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => panic!("sctap1 not released after drop: {e}"),
            }
        };
        assert_eq!(c.plan.name, "sctap1");
        assert!(c.lock_path().ends_with("sctap1.lock"));
    }

    /// A tempdir with mode 0700 (with umask 002 `tempfile` can leave it group-writable,
    /// which the lock-dir parent check rightly refuses).
    fn private_tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
        d
    }

    fn uid() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    #[test]
    fn independent_pools_share_host_wide_leases() {
        // Two backends with different run dirs (two `SANDCRAB_HOME`s, two test state dirs)
        // and the default lock dir: they lease from the same host pool and must never hand
        // out the same tap. A per-process prefix keeps the fake taps clear of real leases.
        let prefix = format!("sct{}x", std::process::id());
        let host = private_tempdir();
        let sysfs = host.path().join("net");
        for i in 0..2 {
            fake_tap(&sysfs, &format!("{prefix}{i}"), &uid().to_string());
        }
        let (home_a, home_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let backend_cfg = |home: &Path| {
            let mut c = crate::FirecrackerConfig::with_home(home, "/unused/crabd");
            c.taps.prefix = prefix.clone();
            c.taps.size = 2;
            c.taps.sysfs_net = sysfs.clone();
            c
        };
        let (a, b) = (backend_cfg(home_a.path()), backend_cfg(home_b.path()));
        assert_ne!(a.run_dir, b.run_dir);
        assert_eq!(a.taps.lock_dir, None);

        let la = acquire_tap(&a.taps, uid()).unwrap();
        let lb = acquire_tap(&b.taps, uid()).unwrap();
        assert_ne!(la.plan.name, lb.plan.name);
        for pool in [&a.taps, &b.taps] {
            let err = acquire_tap(pool, uid()).unwrap_err().to_string();
            assert!(err.contains("in use"), "{err}");
        }
        let dir = default_lock_dir();
        assert_eq!(la.lock_path().parent(), Some(dir.as_path()));
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        let paths = [la.lock_path().to_owned(), lb.lock_path().to_owned()];
        drop((la, lb));
        for p in paths {
            let _ = fs::remove_file(p);
        }
    }

    #[test]
    fn default_lock_dir_is_host_wide() {
        let d = default_lock_dir();
        assert!(d.is_absolute(), "{}", d.display());
        assert!(d.ends_with("sandcrab/taps") || d.ends_with(format!("sandcrab-{}/taps", uid())));
        assert_eq!(d, default_lock_dir(), "independent of any sandcrab home");
    }

    #[test]
    fn refuses_unsafe_lock_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let host = private_tempdir();
        let sysfs = host.path().join("net");
        fake_tap(&sysfs, "sctap0", &uid().to_string());
        let cfg = |lock_dir: PathBuf| TapPoolConfig {
            prefix: "sctap".into(),
            size: 1,
            sysfs_net: sysfs.clone(),
            lock_dir: Some(lock_dir),
        };

        // Group/other-accessible lock dir (e.g. pre-created in /tmp with a loose mode).
        let loose = host.path().join("loose").join("taps");
        fs::create_dir_all(&loose).unwrap();
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o777)).unwrap();
        let err = acquire_tap(&cfg(loose.clone()), uid())
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing tap lock dir"), "{err}");

        // Loose parent: its owner could swap the `taps` dir.
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(loose.parent().unwrap(), fs::Permissions::from_mode(0o775)).unwrap();
        let err = acquire_tap(&cfg(loose.clone()), uid())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not group/other-writable") && err.contains("mode 775"),
            "{err}"
        );
        // A read-only-for-others parent (0755) is fine.
        fs::set_permissions(loose.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            acquire_tap(&cfg(loose.clone()), uid()).unwrap().plan.name,
            "sctap0"
        );

        // A symlinked lock dir.
        let real = host.path().join("real");
        fs::create_dir_all(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let link = host.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = acquire_tap(&cfg(link), uid()).unwrap_err().to_string();
        assert!(err.contains("a symlink"), "{err}");

        // A dir owned by another user (only testable as root: chown to nobody).
        if uid() == 0 {
            let foreign = host.path().join("foreign");
            fs::create_dir_all(&foreign).unwrap();
            fs::set_permissions(&foreign, fs::Permissions::from_mode(0o700)).unwrap();
            std::os::unix::fs::chown(&foreign, Some(65534), Some(65534)).unwrap();
            let err = acquire_tap(&cfg(foreign), uid()).unwrap_err().to_string();
            assert!(err.contains("owned by uid 0"), "{err}");
        }

        // A private dir works.
        let ok = host.path().join("ok").join("taps");
        assert_eq!(acquire_tap(&cfg(ok), uid()).unwrap().plan.name, "sctap0");
    }
}
