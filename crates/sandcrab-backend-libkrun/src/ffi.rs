//! Runtime-loaded libkrun FFI (design §3): a hand-written `extern "C"` fn-pointer table for
//! the libkrun subset we use, loaded with `libloading` from **pinned absolute paths** after the
//! file's SHA-256 has been checked, plus the safe [`Ctx`] wrapper. This module is the crate's
//! only `unsafe` island; the `sandcrab-krun` helper is its only user.
//!
//! The VM configuration is first expressed as a pure, ordered list of [`KrunCall`]s
//! ([`plan`]), then applied one by one with [`Ctx::apply`]. The plan is unit-testable without
//! libkrun, so the security-relevant call sequence (TSI off, empty port map; design §9, risk 1)
//! is asserted in tests, and the helper logs each call as it applies it.
//!
//! libkrunfw: libkrun `dlopen`s it by **leaf name** ([`LIBKRUNFW_LEAF`]) inside
//! `krun_start_enter`. [`Krun::load`] hash-verifies it and loads it first by absolute path
//! (`RTLD_GLOBAL`). On Linux glibc then resolves the leaf name to that already-loaded image by
//! soname. **macOS dyld does not** (observed: "Couldn't find or load libkrunfw.5.dylib" with
//! the image preloaded), so the leaf lookup still searches [`LIBRARY_PATH_ENV`], which dyld
//! reads only at process start. The caller must therefore launch the helper with that variable
//! set to exactly the pinned libkrunfw directory; [`check_libkrunfw_lookup`] enforces it
//! before anything is loaded, so the search can only find the file that was hashed.

use std::ffi::{CString, c_char};
use std::fmt;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::helper_proto::{
    CONTROL_PORT, HelperConfig, HelperNetwork, PinnedFile, TOOLS_PORT, VirtiofsSemantics,
};

/// `KRUN_TSI_HIJACK_INET`: TSI for AF_INET/AF_INET6 sockets.
pub const KRUN_TSI_HIJACK_INET: u32 = 1 << 0;
/// `NET_FLAG_DHCP_CLIENT`: `init.krun` configures `eth0` by DHCP before exec'ing the guest
/// init (Rapid Commit DISCOVER, falls back to the OFFER).
pub const NET_FLAG_DHCP_CLIENT: u32 = 1 << 1;
/// virtio-net features for the userspace NIC: none (no checksum/TSO offloads), so every frame
/// is a complete, checksummed Ethernet frame of at most MTU + 14 bytes.
pub const NET_FEATURES: u32 = 0;
/// Symbol of `krun_add_net_unixstream`: optional at load time, because libkrun builds without
/// the `net` cargo feature lack it; required only for [`HelperNetwork::Userspace`].
pub const ADD_NET_UNIXSTREAM: &str = "krun_add_net_unixstream";
/// `KRUN_SEMANTICS_LINUX_COMPLETE`.
pub const KRUN_SEMANTICS_LINUX_COMPLETE: u32 = 0;
/// `KRUN_SEMANTICS_LINUX_SIMPLIFIED`.
pub const KRUN_SEMANTICS_LINUX_SIMPLIFIED: u32 = 1;
/// Root block device inside the guest: the first `krun_add_disk` becomes `/dev/vda`.
pub const ROOT_DEVICE: &str = "/dev/vda";
/// Root filesystem type and mount options for `krun_set_root_disk_remount`.
pub const ROOT_FSTYPE: &str = "ext4";
pub const ROOT_OPTIONS: &str = "ro";

/// File name libkrun 1.19 `dlopen`s libkrunfw by.
#[cfg(target_os = "macos")]
pub const LIBKRUNFW_LEAF: &str = "libkrunfw.5.dylib";
#[cfg(not(target_os = "macos"))]
pub const LIBKRUNFW_LEAF: &str = "libkrunfw.so.5";
/// Search path variable for that leaf-name lookup; must be exactly the libkrunfw directory.
#[cfg(target_os = "macos")]
pub const LIBRARY_PATH_ENV: &str = "DYLD_FALLBACK_LIBRARY_PATH";
#[cfg(not(target_os = "macos"))]
pub const LIBRARY_PATH_ENV: &str = "LD_LIBRARY_PATH";
/// Variables searched before [`LIBRARY_PATH_ENV`] (would let another file win); must be unset.
#[cfg(target_os = "macos")]
const PRECEDING_PATH_ENVS: &[&str] = &["DYLD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES"];
#[cfg(not(target_os = "macos"))]
const PRECEDING_PATH_ENVS: &[&str] = &["LD_PRELOAD"];

/// Errors from loading or calling libkrun.
#[derive(Debug, thiserror::Error)]
pub enum FfiError {
    #[error("reading `{path}`: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("`{path}` SHA-256 mismatch: expected {expected}, got {actual}")]
    ShaMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("loading `{path}`: {source}")]
    Load {
        path: PathBuf,
        source: libloading::Error,
    },
    #[error("`{path}` lacks required symbol `{symbol}` (libkrun ≥ 1.19 needed): {source}")]
    MissingSymbol {
        path: PathBuf,
        symbol: &'static str,
        source: libloading::Error,
    },
    #[error("libkrunfw lookup: {0}")]
    Lookup(String),
    #[error("invalid argument for {call}: {reason}")]
    InvalidArgument { call: &'static str, reason: String },
    #[error("{call} failed: {} (errno {errno})", std::io::Error::from_raw_os_error(*errno))]
    Call { call: &'static str, errno: i32 },
}

pub type Result<T> = std::result::Result<T, FfiError>;

/// Lowercase hex SHA-256 of a file (streamed).
pub fn sha256_file(path: &Path) -> Result<String> {
    let io = |source| FfiError::Io {
        path: path.to_owned(),
        source,
    };
    let mut file = std::fs::File::open(path).map_err(io)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Check `path` against its pinned SHA-256.
pub fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual == expected {
        Ok(())
    } else {
        Err(FfiError::ShaMismatch {
            path: path.to_owned(),
            expected: expected.to_owned(),
            actual,
        })
    }
}

/// Check that libkrun's leaf-name `dlopen` of libkrunfw can only resolve to `libkrunfw`
/// (the pinned file): its file name is [`LIBKRUNFW_LEAF`], [`LIBRARY_PATH_ENV`] is exactly
/// its directory, and nothing that is searched earlier is set. `env` reads the process
/// environment (injected for tests).
pub fn check_libkrunfw_lookup(
    libkrunfw: &Path,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<()> {
    let err = |m: String| Err(FfiError::Lookup(m));
    if libkrunfw.file_name() != Some(std::ffi::OsStr::new(LIBKRUNFW_LEAF)) {
        return err(format!(
            "`{}` must be named {LIBKRUNFW_LEAF} (the name libkrun loads it by)",
            libkrunfw.display()
        ));
    }
    let Some(dir) = libkrunfw.parent() else {
        return err(format!("`{}` has no directory", libkrunfw.display()));
    };
    for var in PRECEDING_PATH_ENVS {
        if env(var).is_some_and(|v| !v.is_empty()) {
            return err(format!("{var} must not be set for the helper"));
        }
    }
    match env(LIBRARY_PATH_ENV) {
        Some(v) if Path::new(&v) == dir => Ok(()),
        Some(v) => err(format!(
            "{LIBRARY_PATH_ENV} is `{}`, must be exactly `{}`",
            Path::new(&v).display(),
            dir.display()
        )),
        None => err(format!(
            "{LIBRARY_PATH_ENV} is unset; the helper must be launched with it set to `{}`",
            dir.display()
        )),
    }
}

// ---- the C API subset (libkrun.h, v1.19) -------------------------------------------------

type FnSetLogLevel = unsafe extern "C" fn(level: u32) -> i32;
type FnCreateCtx = unsafe extern "C" fn() -> i32;
type FnFreeCtx = unsafe extern "C" fn(ctx: u32) -> i32;
type FnSetVmConfig = unsafe extern "C" fn(ctx: u32, vcpus: u8, ram_mib: u32) -> i32;
type FnAddDisk = unsafe extern "C" fn(
    ctx: u32,
    block_id: *const c_char,
    disk_path: *const c_char,
    read_only: bool,
) -> i32;
type FnSetRootDiskRemount = unsafe extern "C" fn(
    ctx: u32,
    device: *const c_char,
    fstype: *const c_char,
    options: *const c_char,
) -> i32;
type FnAddVirtiofs4 = unsafe extern "C" fn(
    ctx: u32,
    tag: *const c_char,
    path: *const c_char,
    shm_size: u64,
    read_only: bool,
    semantics: u32,
) -> i32;
type FnDisableImplicitVsock = unsafe extern "C" fn(ctx: u32) -> i32;
type FnAddVsock = unsafe extern "C" fn(ctx: u32, tsi_features: u32) -> i32;
type FnAddVsockPort2 =
    unsafe extern "C" fn(ctx: u32, port: u32, path: *const c_char, listen: bool) -> i32;
type FnSetPortMap = unsafe extern "C" fn(ctx: u32, port_map: *const *const c_char) -> i32;
type FnSetWorkdir = unsafe extern "C" fn(ctx: u32, workdir: *const c_char) -> i32;
type FnSetExec = unsafe extern "C" fn(
    ctx: u32,
    exec_path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> i32;
type FnAddNetUnixstream = unsafe extern "C" fn(
    ctx: u32,
    path: *const c_char,
    fd: i32,
    mac: *const u8,
    features: u32,
    flags: u32,
) -> i32;
type FnGetMaxVcpus = unsafe extern "C" fn() -> i32;
type FnStartEnter = unsafe extern "C" fn(ctx: u32) -> i32;

/// The loaded libkrun function table. Keeps both libraries mapped for its lifetime.
pub struct Krun {
    set_log_level: FnSetLogLevel,
    create_ctx: FnCreateCtx,
    free_ctx: FnFreeCtx,
    set_vm_config: FnSetVmConfig,
    add_disk: FnAddDisk,
    set_root_disk_remount: FnSetRootDiskRemount,
    add_virtiofs4: FnAddVirtiofs4,
    disable_implicit_vsock: FnDisableImplicitVsock,
    add_vsock: FnAddVsock,
    add_vsock_port2: FnAddVsockPort2,
    set_port_map: FnSetPortMap,
    set_workdir: FnSetWorkdir,
    set_exec: FnSetExec,
    get_max_vcpus: FnGetMaxVcpus,
    start_enter: FnStartEnter,
    /// `None` when libkrun was built without the `net` feature.
    add_net_unixstream: Option<FnAddNetUnixstream>,
    libkrun_path: PathBuf,
    _libkrun: libloading::Library,
    _libkrunfw: libloading::Library,
}

impl fmt::Debug for Krun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Krun")
            .field("libkrun", &self.libkrun_path)
            .finish_non_exhaustive()
    }
}

/// `dlopen` an already hash-verified absolute path with `RTLD_NOW | RTLD_GLOBAL`.
fn dlopen_verified(pin: &PinnedFile) -> Result<libloading::Library> {
    if !pin.path.is_absolute() {
        return Err(FfiError::InvalidArgument {
            call: "dlopen",
            reason: format!("`{}` is not an absolute path", pin.path.display()),
        });
    }
    let flags = libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_GLOBAL;
    // SAFETY: loading runs the library's initializers. The file is the pinned, hash-verified
    // libkrun/libkrunfw build, loaded by absolute path (no search-path lookup), in the
    // single-purpose helper process before any other threads touch it.
    let lib = unsafe { libloading::os::unix::Library::open(Some(&pin.path), flags) }.map_err(
        |source| FfiError::Load {
            path: pin.path.clone(),
            source,
        },
    )?;
    Ok(lib.into())
}

/// Resolve one symbol as a copied fn pointer of type `T`.
///
/// # Safety
/// `T` must be the exact C signature of `symbol` in the loaded library, and the returned
/// pointer must not outlive `lib`.
unsafe fn sym<T: Copy>(lib: &libloading::Library, path: &Path, symbol: &'static str) -> Result<T> {
    let mut name = Vec::with_capacity(symbol.len() + 1);
    name.extend_from_slice(symbol.as_bytes());
    name.push(0);
    // SAFETY: forwarded to the caller (signature matches, lifetime bounded by `lib`).
    unsafe { lib.get::<T>(&name) }
        .map(|s| *s)
        .map_err(|source| FfiError::MissingSymbol {
            path: path.to_owned(),
            symbol,
            source,
        })
}

impl Krun {
    /// Verify and load libkrunfw, then verify and load libkrun and resolve every symbol.
    /// Fails if a hash differs (before anything is loaded) or any symbol is missing.
    pub fn load(libkrun: &PinnedFile, libkrunfw: &PinnedFile) -> Result<Self> {
        // Both hashes before either dlopen: nothing runs unless both are the pinned builds.
        verify_sha256(&libkrunfw.path, &libkrunfw.sha256)?;
        verify_sha256(&libkrun.path, &libkrun.sha256)?;
        let fw = dlopen_verified(libkrunfw)?;
        let lib = dlopen_verified(libkrun)?;
        let p = libkrun.path.as_path();
        // SAFETY: each type alias is the exact prototype from libkrun.h v1.19; the pointers
        // are stored next to `_libkrun`, which keeps the library mapped as long as `Krun`.
        unsafe {
            Ok(Self {
                set_log_level: sym(&lib, p, "krun_set_log_level")?,
                create_ctx: sym(&lib, p, "krun_create_ctx")?,
                free_ctx: sym(&lib, p, "krun_free_ctx")?,
                set_vm_config: sym(&lib, p, "krun_set_vm_config")?,
                add_disk: sym(&lib, p, "krun_add_disk")?,
                set_root_disk_remount: sym(&lib, p, "krun_set_root_disk_remount")?,
                add_virtiofs4: sym(&lib, p, "krun_add_virtiofs4")?,
                disable_implicit_vsock: sym(&lib, p, "krun_disable_implicit_vsock")?,
                add_vsock: sym(&lib, p, "krun_add_vsock")?,
                add_vsock_port2: sym(&lib, p, "krun_add_vsock_port2")?,
                set_port_map: sym(&lib, p, "krun_set_port_map")?,
                set_workdir: sym(&lib, p, "krun_set_workdir")?,
                set_exec: sym(&lib, p, "krun_set_exec")?,
                get_max_vcpus: sym(&lib, p, "krun_get_max_vcpus")?,
                start_enter: sym(&lib, p, "krun_start_enter")?,
                add_net_unixstream: sym(&lib, p, ADD_NET_UNIXSTREAM).ok(),
                libkrun_path: libkrun.path.clone(),
                _libkrun: lib,
                _libkrunfw: fw,
            })
        }
    }

    /// Whether this libkrun build exports `krun_add_net_unixstream` (the `net` feature).
    #[must_use]
    pub fn has_net(&self) -> bool {
        self.add_net_unixstream.is_some()
    }

    /// `krun_set_log_level` (global, call before creating a context).
    pub fn set_log_level(&self, level: u32) -> Result<()> {
        // SAFETY: plain integer argument.
        check("krun_set_log_level", unsafe { (self.set_log_level)(level) })
    }

    /// `krun_get_max_vcpus`.
    pub fn max_vcpus(&self) -> Result<u32> {
        // SAFETY: no arguments.
        let n = unsafe { (self.get_max_vcpus)() };
        check("krun_get_max_vcpus", n)?;
        Ok(n.unsigned_abs())
    }

    /// `krun_create_ctx`.
    pub fn create_ctx(&self) -> Result<Ctx<'_>> {
        // SAFETY: no arguments.
        let id = unsafe { (self.create_ctx)() };
        check("krun_create_ctx", id)?;
        Ok(Ctx {
            krun: self,
            id: id.unsigned_abs(),
        })
    }
}

fn check(call: &'static str, ret: i32) -> Result<()> {
    if ret < 0 {
        Err(FfiError::Call {
            call,
            errno: ret.saturating_neg(),
        })
    } else {
        Ok(())
    }
}

fn path_cstr(call: &'static str, p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| FfiError::InvalidArgument {
        call,
        reason: format!("path `{}` contains NUL", p.display()),
    })
}

/// A string for libkrun. Defense in depth over `helper_proto` validation: printable ASCII
/// without `"`/`\` (libkrun aborts the process on a newline in cmdline-bound strings).
fn str_cstr(call: &'static str, s: &str) -> Result<CString> {
    if let Some(c) = s
        .chars()
        .find(|&c| !(' '..='~').contains(&c) || c == '"' || c == '\\')
    {
        return Err(FfiError::InvalidArgument {
            call,
            reason: format!("{c:?} in {s:?} (printable ASCII without quotes/backslash only)"),
        });
    }
    CString::new(s).map_err(|_| FfiError::InvalidArgument {
        call,
        reason: "NUL in string".into(),
    })
}

/// A `NULL`-terminated `char *[]` over owned strings.
struct CStrArray {
    _owned: Vec<CString>,
    ptrs: Vec<*const c_char>,
}

impl CStrArray {
    fn new(call: &'static str, items: &[String]) -> Result<Self> {
        let owned = items
            .iter()
            .map(|s| str_cstr(call, s))
            .collect::<Result<Vec<_>>>()?;
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        Ok(Self {
            _owned: owned,
            ptrs,
        })
    }

    fn as_ptr(&self) -> *const *const c_char {
        self.ptrs.as_ptr()
    }
}

/// One libkrun configuration call, in plan order. `Display` renders it as the C call (the
/// helper logs exactly this line before applying it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KrunCall {
    SetVmConfig {
        vcpus: u8,
        ram_mib: u32,
    },
    AddDisk {
        block_id: String,
        path: PathBuf,
        read_only: bool,
    },
    SetRootDiskRemount {
        device: String,
        fstype: String,
        options: String,
    },
    AddVirtiofs {
        tag: String,
        path: PathBuf,
        read_only: bool,
        semantics: VirtiofsSemantics,
    },
    DisableImplicitVsock,
    AddVsock {
        tsi_features: u32,
    },
    /// A virtio-net NIC on an already-connected unixstream socket (the helper's egress
    /// filter). [`plan`] emits `fd: -1`; the helper substitutes its socket fd, whose ownership
    /// then passes to libkrun.
    AddNetUnixstream {
        fd: i32,
        mac: [u8; 6],
        features: u32,
        flags: u32,
    },
    /// Always empty: an empty array publishes no guest port (`NULL` would publish all of
    /// them on the host wildcard address, design §9).
    SetPortMap(Vec<String>),
    AddVsockPort {
        port: u32,
        path: PathBuf,
        listen: bool,
    },
    SetWorkdir(String),
    SetExec {
        path: String,
        args: Vec<String>,
        env: Vec<String>,
    },
}

impl KrunCall {
    /// The C function this call maps to.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::SetVmConfig { .. } => "krun_set_vm_config",
            Self::AddDisk { .. } => "krun_add_disk",
            Self::SetRootDiskRemount { .. } => "krun_set_root_disk_remount",
            Self::AddVirtiofs { .. } => "krun_add_virtiofs4",
            Self::DisableImplicitVsock => "krun_disable_implicit_vsock",
            Self::AddVsock { .. } => "krun_add_vsock",
            Self::AddNetUnixstream { .. } => "krun_add_net_unixstream",
            Self::SetPortMap(_) => "krun_set_port_map",
            Self::AddVsockPort { .. } => "krun_add_vsock_port2",
            Self::SetWorkdir(_) => "krun_set_workdir",
            Self::SetExec { .. } => "krun_set_exec",
        }
    }
}

impl fmt::Display for KrunCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.name();
        match self {
            Self::SetVmConfig { vcpus, ram_mib } => {
                write!(f, "{n}(vcpus={vcpus}, ram_mib={ram_mib})")
            }
            Self::AddDisk {
                block_id,
                path,
                read_only,
            } => write!(f, "{n}({block_id:?}, {path:?}, read_only={read_only})"),
            Self::SetRootDiskRemount {
                device,
                fstype,
                options,
            } => write!(f, "{n}({device:?}, {fstype:?}, {options:?})"),
            Self::AddVirtiofs {
                tag,
                path,
                read_only,
                semantics,
            } => write!(
                f,
                "{n}({tag:?}, {path:?}, shm_size=0, read_only={read_only}, semantics={})",
                semantics_raw(*semantics)
            ),
            Self::DisableImplicitVsock => write!(f, "{n}()"),
            Self::AddVsock { tsi_features } => write!(f, "{n}(tsi_features={tsi_features})"),
            Self::AddNetUnixstream {
                fd,
                mac,
                features,
                flags,
            } => {
                let mac: Vec<String> = mac.iter().map(|b| format!("{b:02x}")).collect();
                write!(
                    f,
                    "{n}(path=NULL, fd={fd}, mac={}, features={features:#x}, flags={flags:#x})",
                    mac.join(":")
                )
            }
            Self::SetPortMap(m) => write!(f, "{n}({m:?})"),
            Self::AddVsockPort { port, path, listen } => {
                write!(f, "{n}({port}, {path:?}, listen={listen})")
            }
            Self::SetWorkdir(w) => write!(f, "{n}({w:?})"),
            Self::SetExec { path, args, env } => write!(f, "{n}({path:?}, {args:?}, {env:?})"),
        }
    }
}

fn semantics_raw(s: VirtiofsSemantics) -> u32 {
    match s {
        VirtiofsSemantics::LinuxComplete => KRUN_SEMANTICS_LINUX_COMPLETE,
        VirtiofsSemantics::LinuxSimplified => KRUN_SEMANTICS_LINUX_SIMPLIFIED,
    }
}

/// The ordered configuration calls for `cfg` (between `krun_create_ctx` and
/// `krun_start_enter`). Pure: no I/O, no libkrun.
///
/// Order: VM size → disks (root first, so it is `/dev/vda`; then scratch; then workspace
/// volumes) → block-root remount → virtio-fs shares → vsock device (TSI off unless the config
/// opts in) → empty port map → userspace NIC (`userspace` only; libkrun refuses a port map
/// after it) → vsock port → UDS mappings
/// → workdir → exec.
#[must_use]
pub fn plan(cfg: &HelperConfig) -> Vec<KrunCall> {
    let mut calls = vec![KrunCall::SetVmConfig {
        vcpus: cfg.vcpus,
        ram_mib: cfg.memory_mib,
    }];
    for d in [&cfg.root, &cfg.scratch].into_iter().chain(&cfg.disks) {
        calls.push(KrunCall::AddDisk {
            block_id: d.block_id.clone(),
            path: d.path.clone(),
            read_only: d.read_only,
        });
    }
    calls.push(KrunCall::SetRootDiskRemount {
        device: ROOT_DEVICE.into(),
        fstype: ROOT_FSTYPE.into(),
        options: ROOT_OPTIONS.into(),
    });
    for s in &cfg.virtiofs {
        calls.push(KrunCall::AddVirtiofs {
            tag: s.tag.clone(),
            path: s.host_path.clone(),
            read_only: s.read_only,
            semantics: cfg.virtiofs_semantics,
        });
    }
    let tsi_features = match cfg.network {
        HelperNetwork::Disabled | HelperNetwork::Userspace => 0,
        HelperNetwork::TsiUnrestricted => KRUN_TSI_HIJACK_INET,
    };
    calls.extend([
        KrunCall::DisableImplicitVsock,
        KrunCall::AddVsock { tsi_features },
        KrunCall::SetPortMap(Vec::new()),
    ]);
    if cfg.network == HelperNetwork::Userspace {
        // After the (empty) port map: libkrun refuses `krun_set_port_map` once a NIC exists
        // (`net_index != 0` → EINVAL).
        calls.push(KrunCall::AddNetUnixstream {
            fd: -1,
            mac: crate::netstack::GUEST_MAC,
            features: NET_FEATURES,
            flags: NET_FLAG_DHCP_CLIENT,
        });
    }
    calls.extend([
        KrunCall::AddVsockPort {
            port: CONTROL_PORT,
            path: cfg.control_socket.clone(),
            listen: true,
        },
        KrunCall::AddVsockPort {
            port: TOOLS_PORT,
            path: cfg.tools_socket.clone(),
            listen: true,
        },
        KrunCall::SetWorkdir(cfg.exec.workdir.clone()),
        KrunCall::SetExec {
            path: cfg.exec.path.clone(),
            args: cfg.exec.args.clone(),
            env: cfg.exec.env.clone(),
        },
    ]);
    calls
}

/// A libkrun configuration context. Freed on drop unless consumed by
/// [`Ctx::start_enter`].
#[derive(Debug)]
pub struct Ctx<'k> {
    krun: &'k Krun,
    id: u32,
}

impl Ctx<'_> {
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Apply one planned call.
    pub fn apply(&self, call: &KrunCall) -> Result<()> {
        let k = self.krun;
        let n = call.name();
        let id = self.id;
        // SAFETY (all arms): `id` is a live context from `krun_create_ctx`; every pointer is
        // a NUL-terminated C string or NULL-terminated array owned by a local that outlives
        // the call; libkrun copies what it keeps. Prototypes match libkrun.h v1.19.
        let ret = match call {
            KrunCall::SetVmConfig { vcpus, ram_mib } => unsafe {
                (k.set_vm_config)(id, *vcpus, *ram_mib)
            },
            KrunCall::AddDisk {
                block_id,
                path,
                read_only,
            } => {
                let b = str_cstr(n, block_id)?;
                let p = path_cstr(n, path)?;
                unsafe { (k.add_disk)(id, b.as_ptr(), p.as_ptr(), *read_only) }
            }
            KrunCall::SetRootDiskRemount {
                device,
                fstype,
                options,
            } => {
                let d = str_cstr(n, device)?;
                let t = str_cstr(n, fstype)?;
                let o = str_cstr(n, options)?;
                unsafe { (k.set_root_disk_remount)(id, d.as_ptr(), t.as_ptr(), o.as_ptr()) }
            }
            KrunCall::AddVirtiofs {
                tag,
                path,
                read_only,
                semantics,
            } => {
                let t = str_cstr(n, tag)?;
                let p = path_cstr(n, path)?;
                let sem = semantics_raw(*semantics);
                unsafe { (k.add_virtiofs4)(id, t.as_ptr(), p.as_ptr(), 0, *read_only, sem) }
            }
            KrunCall::DisableImplicitVsock => unsafe { (k.disable_implicit_vsock)(id) },
            KrunCall::AddVsock { tsi_features } => unsafe { (k.add_vsock)(id, *tsi_features) },
            KrunCall::AddNetUnixstream {
                fd,
                mac,
                features,
                flags,
            } => {
                let Some(add) = k.add_net_unixstream else {
                    return Err(FfiError::InvalidArgument {
                        call: n,
                        reason: format!(
                            "`{}` lacks {ADD_NET_UNIXSTREAM}: rebuild libkrun with NET=1 \
                             (scripts/fetch-libkrun.sh)",
                            k.libkrun_path.display()
                        ),
                    });
                };
                if *fd < 0 {
                    return Err(FfiError::InvalidArgument {
                        call: n,
                        reason: "no netstack socket fd".into(),
                    });
                }
                // libkrun takes ownership of `fd` (path is NULL, mac is 6 bytes it copies).
                unsafe { add(id, std::ptr::null(), *fd, mac.as_ptr(), *features, *flags) }
            }
            KrunCall::SetPortMap(map) => {
                let a = CStrArray::new(n, map)?;
                unsafe { (k.set_port_map)(id, a.as_ptr()) }
            }
            KrunCall::AddVsockPort { port, path, listen } => {
                let p = path_cstr(n, path)?;
                unsafe { (k.add_vsock_port2)(id, *port, p.as_ptr(), *listen) }
            }
            KrunCall::SetWorkdir(w) => {
                let w = str_cstr(n, w)?;
                unsafe { (k.set_workdir)(id, w.as_ptr()) }
            }
            KrunCall::SetExec { path, args, env } => {
                let p = str_cstr(n, path)?;
                let a = CStrArray::new(n, args)?;
                let e = CStrArray::new(n, env)?;
                unsafe { (k.set_exec)(id, p.as_ptr(), a.as_ptr(), e.as_ptr()) }
            }
        };
        check(n, ret)
    }

    /// `krun_start_enter`. On success it never returns: libkrun runs the VM and `exit()`s the
    /// process with the guest's status. Returning means it failed before starting.
    pub fn start_enter(self) -> FfiError {
        let me = std::mem::ManuallyDrop::new(self);
        // SAFETY: `id` is a live, fully configured context; libkrun consumes it (so it is
        // not freed afterwards).
        let ret = unsafe { (me.krun.start_enter)(me.id) };
        FfiError::Call {
            call: "krun_start_enter",
            errno: if ret < 0 { ret.saturating_neg() } else { 0 },
        }
    }
}

impl Drop for Ctx<'_> {
    fn drop(&mut self) {
        // SAFETY: `id` is a live context not consumed by `start_enter`.
        let _ = unsafe { (self.krun.free_ctx)(self.id) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helper_proto::{
        DiskSpec, GuestExec, HELPER_PROTO_VERSION, ROOT_DRIVE_ID, SCRATCH_DRIVE_ID, VirtioFsShare,
    };
    use std::io::Write;

    const SHA_ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn cfg() -> HelperConfig {
        HelperConfig {
            version: HELPER_PROTO_VERSION,
            libkrun: PinnedFile {
                path: "/a/libkrun.dylib".into(),
                sha256: SHA_ZERO.into(),
            },
            libkrunfw: PinnedFile {
                path: "/a/libkrunfw.5.dylib".into(),
                sha256: SHA_ZERO.into(),
            },
            vcpus: 2,
            memory_mib: 512,
            root: DiskSpec {
                block_id: ROOT_DRIVE_ID.into(),
                path: "/c/rootfs.ext4".into(),
                read_only: true,
                sha256: None,
            },
            scratch: DiskSpec {
                block_id: SCRATCH_DRIVE_ID.into(),
                path: "/r/id/scratch.ext4".into(),
                read_only: false,
                sha256: None,
            },
            disks: vec![],
            virtiofs: vec![VirtioFsShare {
                tag: "ws0".into(),
                host_path: "/w/app".into(),
                read_only: true,
                identity: None,
            }],
            virtiofs_semantics: VirtiofsSemantics::LinuxComplete,
            control_socket: "/r/id/ctl.sock".into(),
            tools_socket: "/r/id/v.sock".into(),
            network: HelperNetwork::Disabled,
            netstack: None,
            exec: GuestExec::default(),
            log_level: 0,
        }
    }

    #[test]
    fn userspace_network_adds_one_nic_and_no_tsi() {
        let mut c = cfg();
        c.network = HelperNetwork::Userspace;
        c.netstack = Some(crate::helper_proto::NetstackSpec {
            kind: crate::netstack::NetstackKind::Gvproxy,
            binary: PinnedFile {
                path: "/a/gvproxy".into(),
                sha256: SHA_ZERO.into(),
            },
            deny: vec![],
        });
        c.validate().unwrap();
        let rendered: Vec<String> = plan(&c).iter().map(ToString::to_string).collect();
        let at = |s: &str| rendered.iter().position(|x| x.starts_with(s)).unwrap();
        assert_eq!(
            rendered[at("krun_add_vsock(")],
            "krun_add_vsock(tsi_features=0)"
        );
        assert_eq!(
            rendered[at("krun_add_net_unixstream")],
            "krun_add_net_unixstream(path=NULL, fd=-1, mac=5a:94:ef:e4:0c:ee, features=0x0, flags=0x2)"
        );
        assert!(at("krun_disable_implicit_vsock") < at("krun_add_vsock("));
        assert!(at("krun_set_port_map([])") < at("krun_add_net_unixstream"));
        assert_eq!(
            rendered
                .iter()
                .filter(|x| x.starts_with("krun_add_net"))
                .count(),
            1
        );
        // No NIC without `userspace`.
        for net in [HelperNetwork::Disabled, HelperNetwork::TsiUnrestricted] {
            c.network = net;
            assert!(
                !plan(&c)
                    .iter()
                    .any(|x| matches!(x, KrunCall::AddNetUnixstream { .. }))
            );
        }
    }

    #[test]
    fn disabled_network_call_sequence() {
        let c = cfg();
        c.validate().unwrap();
        let rendered: Vec<String> = plan(&c).iter().map(ToString::to_string).collect();
        assert_eq!(
            rendered,
            [
                r#"krun_set_vm_config(vcpus=2, ram_mib=512)"#,
                r#"krun_add_disk("rootfs", "/c/rootfs.ext4", read_only=true)"#,
                r#"krun_add_disk("scratch", "/r/id/scratch.ext4", read_only=false)"#,
                r#"krun_set_root_disk_remount("/dev/vda", "ext4", "ro")"#,
                r#"krun_add_virtiofs4("ws0", "/w/app", shm_size=0, read_only=true, semantics=0)"#,
                r#"krun_disable_implicit_vsock()"#,
                r#"krun_add_vsock(tsi_features=0)"#,
                r#"krun_set_port_map([])"#,
                r#"krun_add_vsock_port2(1, "/r/id/ctl.sock", listen=true)"#,
                r#"krun_add_vsock_port2(1024, "/r/id/v.sock", listen=true)"#,
                r#"krun_set_workdir("/")"#,
                r#"krun_set_exec("/sbin/crabd", ["init"], ["KRUN_INIT_PID1=1", "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"])"#,
            ]
        );
    }

    #[test]
    fn tsi_only_with_explicit_opt_in_and_never_a_port_map() {
        let mut c = cfg();
        for (net, features) in [
            (HelperNetwork::Disabled, 0),
            (HelperNetwork::TsiUnrestricted, KRUN_TSI_HIJACK_INET),
        ] {
            c.network = net;
            let calls = plan(&c);
            let pos = |want: &KrunCall| calls.iter().position(|x| x == want).unwrap();
            let disable = pos(&KrunCall::DisableImplicitVsock);
            let add = pos(&KrunCall::AddVsock {
                tsi_features: features,
            });
            assert!(disable < add, "implicit vsock disabled before adding ours");
            assert_eq!(
                calls
                    .iter()
                    .filter(|x| matches!(x, KrunCall::AddVsock { .. }))
                    .count(),
                1
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|x| matches!(x, KrunCall::SetPortMap(m) if m.is_empty()))
                    .count(),
                1,
                "exactly one, empty port map"
            );
        }
    }

    #[test]
    fn disks_order_and_semantics() {
        let mut c = cfg();
        c.disks.push(DiskSpec {
            block_id: "ws-vol".into(),
            path: "/v/vol.ext4".into(),
            read_only: false,
            sha256: None,
        });
        c.virtiofs_semantics = VirtiofsSemantics::LinuxSimplified;
        let ids: Vec<_> = plan(&c)
            .into_iter()
            .filter_map(|x| match x {
                KrunCall::AddDisk { block_id, .. } => Some(block_id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [ROOT_DRIVE_ID, SCRATCH_DRIVE_ID, "ws-vol"]);
        assert!(
            plan(&c)
                .iter()
                .any(|x| x.to_string().ends_with("semantics=1)"))
        );
    }

    #[test]
    fn strings_are_checked_before_reaching_c() {
        assert!(str_cstr("t", "ok value=1").is_ok());
        for bad in ["a\nb", "a\0b", "q\"", "b\\", "é"] {
            assert!(
                matches!(str_cstr("t", bad), Err(FfiError::InvalidArgument { .. })),
                "{bad:?}"
            );
        }
        assert!(path_cstr("t", Path::new("/a\0b")).is_err());
        assert!(path_cstr("t", Path::new("/Users/josé/app")).is_ok());
        let a = CStrArray::new("t", &["x".into(), "y".into()]).unwrap();
        assert_eq!(a.ptrs.len(), 3);
        assert!(a.ptrs[2].is_null());
    }

    #[test]
    fn sha_is_checked_before_dlopen() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("libkrun.fake");
        std::fs::File::create(&fake)
            .unwrap()
            .write_all(b"not a library")
            .unwrap();
        let actual = sha256_file(&fake).unwrap();
        assert_eq!(actual.len(), 64);
        let wrong = PinnedFile {
            path: fake.clone(),
            sha256: SHA_ZERO.into(),
        };
        let right = PinnedFile {
            path: fake.clone(),
            sha256: actual,
        };
        // Wrong pin on either file: rejected by hash, nothing loaded.
        assert!(matches!(
            Krun::load(&wrong, &right),
            Err(FfiError::ShaMismatch { .. })
        ));
        assert!(matches!(
            Krun::load(&right, &wrong),
            Err(FfiError::ShaMismatch { .. })
        ));
        // Right pin: the hash passes and only then dlopen runs (and rejects the junk).
        assert!(matches!(
            Krun::load(&right, &right),
            Err(FfiError::Load { .. })
        ));
        // Missing file.
        let missing = PinnedFile {
            path: dir.path().join("nope"),
            sha256: SHA_ZERO.into(),
        };
        assert!(matches!(
            Krun::load(&missing, &right),
            Err(FfiError::Io { .. })
        ));
    }

    #[test]
    fn libkrunfw_lookup_is_pinned_to_its_dir() {
        use std::collections::HashMap;
        use std::ffi::OsString;
        let fw = PathBuf::from("/pins/krun").join(LIBKRUNFW_LEAF);
        let run = |vars: &[(&str, &str)]| {
            let m: HashMap<String, OsString> = vars
                .iter()
                .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
                .collect();
            check_libkrunfw_lookup(&fw, |k| m.get(k).cloned())
        };
        run(&[(LIBRARY_PATH_ENV, "/pins/krun")]).unwrap();
        run(&[(LIBRARY_PATH_ENV, "/pins/krun/")]).unwrap();
        for bad in [
            vec![],
            vec![(LIBRARY_PATH_ENV, "/opt/homebrew/lib")],
            vec![(LIBRARY_PATH_ENV, "/elsewhere:/pins/krun")],
            vec![
                (LIBRARY_PATH_ENV, "/pins/krun"),
                (PRECEDING_PATH_ENVS[0], "/evil"),
            ],
        ] {
            assert!(matches!(run(&bad), Err(FfiError::Lookup(_))), "{bad:?}");
        }
        let renamed = Path::new("/pins/krun/libkrunfw.dylib");
        assert!(check_libkrunfw_lookup(renamed, |_| Some("/pins/krun".into())).is_err());
    }

    #[test]
    fn errno_mapping() {
        assert!(check("x", 0).is_ok());
        assert!(check("x", 7).is_ok());
        let e = check("krun_add_disk", -22).unwrap_err();
        assert!(matches!(e, FfiError::Call { errno: 22, .. }));
        assert!(e.to_string().starts_with("krun_add_disk failed"), "{e}");
    }
}
