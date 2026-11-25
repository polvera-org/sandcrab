//! Pinned libkrun / libkrunfw per (os, arch): resolution, SHA-256 checks, install support
//! (design §3, §5, §17 risk 4, §18.2).
//!
//! # Layout
//!
//! The installer ([`install_homebrew`] / [`install_files`], or `scripts/fetch-libkrun.sh`)
//! writes one directory per pinned libkrun version:
//!
//! ```text
//! <SANDCRAB_HOME>/artifacts/libkrun/<LIBKRUN_VERSION>/
//!   manifest.json            # Manifest: installed shas bound to their pinned provenance
//!   libkrun.1.dylib          # macOS arm64   | libkrun.so.1   on Linux x86_64
//!   libkrunfw.5.dylib        # macOS arm64   | libkrunfw.so.5 on Linux x86_64
//! ```
//!
//! # What is pinned
//!
//! Pins are compiled in ([`pins_for`]) and cover *provenance*: the Homebrew bottle file shas,
//! the libkrunfw prebuilt release asset (tarball + member sha) and the upstream git tag
//! commits for from-source builds. Install fix-ups (below) and from-source builds are not
//! byte-reproducible across toolchains, so the installed file's sha is recorded in
//! `manifest.json` together with the provenance it came from. [`Artifacts::resolve`] accepts
//! an install only when every link of that chain matches a compiled-in pin:
//!
//! - manifest os/arch/versions/file names = this build's pins;
//! - `origin = homebrew | release` → `origin_sha256` = the pinned bottle / asset member sha, and
//!   with no fix-ups the installed sha must equal it byte for byte;
//! - `origin = source` → `git_commit` = the pinned tag commit;
//! - the file on disk hashes to the manifest's `sha256`.
//!
//! Anything else (no pins for this platform, missing manifest, an origin this platform has no
//! pin for, a digest mismatch) is a hard error. Nothing is loaded unverified, and we never
//! search `/opt/homebrew`, `LD_LIBRARY_PATH` or `DYLD_*` implicitly (design §3).
//!
//! # libkrunfw lookup (install-name handling)
//!
//! libkrun `dlopen`s libkrunfw **by leaf name** (`libkrunfw.5.dylib` / `libkrunfw.so.5`)
//! [spike]. The helper therefore loads the pinned libkrunfw **first, by absolute path**, and
//! keeps the handle alive; libkrun's later leaf-name `dlopen` then resolves to that image:
//!
//! - **macOS:** dyld matches a leaf-name `dlopen` against already-loaded images by *install
//!   name*. The bottle's id is `/opt/homebrew/opt/libkrunfw/lib/libkrunfw.5.dylib`, which does
//!   not match, so the installer rewrites the copy's id to the leaf `libkrunfw.5.dylib`
//!   (`install_name_tool -id`) and re-signs it ad hoc (`codesign -f -s -`). libkrun itself is
//!   copied unmodified (its installed sha *is* the bottle sha). Its `LC_LOAD_DYLIB`s on
//!   `/opt/homebrew/opt/{molten-vk,libepoxy}` stay: only the unused GPU path needs them, but they
//!   must exist at load time (design §17 risk 4).
//! - **Linux:** glibc matches a no-slash `dlopen` against loaded objects by `DT_SONAME`
//!   (`libkrunfw.so.5`), so pre-loading suffices. From-source `libkrun.so.1` also gets
//!   `RUNPATH=$ORIGIN` (`patchelf`) so it finds its sibling even without the pre-load.
//!
//! # API for the helper (`ffi.rs`)
//!
//! ```ignore
//! let a = Artifacts::resolve(&config)?;                 // verifies the whole chain
//! let fw = load(&a.libkrunfw.path, &a.libkrunfw.sha256)?; // re-hash → dlopen, keep alive
//! let krun = Krun::load(&a.libkrun.path, &a.libkrun.sha256)?;
//! ```
//!
//! [`PinnedPath::verify`] re-hashes a file against its expected sha (use it right before
//! `dlopen`). With `LibkrunConfig.verify_pins = false` the manifest and pins are skipped and the
//! expected shas are the files' current digests (still guarding resolve → load).

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::LibkrunConfig;

/// Pinned libkrun release (Homebrew `libkrun` 1.19.8, upstream tag `v1.19.8`).
pub const LIBKRUN_VERSION: &str = "1.19.8";
/// Pinned libkrunfw release (Homebrew `libkrunfw` 5.6.4, upstream tag `v5.6.4`, kernel 6.12.115).
pub const LIBKRUNFW_VERSION: &str = "5.6.4";
/// Manifest file name inside the artifact dir.
pub const MANIFEST_NAME: &str = "manifest.json";
/// Manifest schema version.
pub const MANIFEST_SCHEMA: u32 = 1;

/// Upstream source pin: a git tag and the commit it must resolve to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitPin {
    pub repo: &'static str,
    pub tag: &'static str,
    pub commit: &'static str,
}

pub const LIBKRUN_GIT: GitPin = GitPin {
    repo: "https://github.com/libkrun/libkrun",
    tag: "v1.19.8",
    commit: "c4d283c233bc0ae1993c289ec2a1b4da2242f1f8",
};

pub const LIBKRUNFW_GIT: GitPin = GitPin {
    repo: "https://github.com/libkrun/libkrunfw",
    tag: "v5.6.4",
    commit: "b31ee3033103aaabdfef63e3d12a6ada873ace92",
};

/// A pinned release download (tarball digest; member digest if a file is extracted from it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetPin {
    pub url: &'static str,
    pub sha256: &'static str,
    /// Path of the library inside the tarball (`None` for build inputs).
    pub member: Option<&'static str>,
    pub member_sha256: Option<&'static str>,
}

/// libkrunfw's pre-generated kernel bundle (`kernel.c`) for aarch64: the macOS from-source
/// build compiles it into the dylib instead of building the kernel (what Homebrew does).
pub const LIBKRUNFW_PREBUILT_KERNEL_AARCH64: AssetPin = AssetPin {
    url: "https://github.com/libkrun/libkrunfw/releases/download/v5.6.4/libkrunfw-prebuilt-aarch64.tgz",
    sha256: "db126ed9775e0f92fc910a90a8b7ff52bdf55644b5edc75977115f81603e48c9",
    member: None,
    member_sha256: None,
};

/// libkrunfw's kernel source (`KERNEL_REMOTE` in its Makefile, which would fetch it unverified).
/// `--fw-from-source` (Linux) pre-seeds `tarballs/` with this verified copy.
pub const LIBKRUNFW_KERNEL_SOURCE: AssetPin = AssetPin {
    url: "https://github.com/libkrun/linux/archive/refs/tags/v5.6.4-6.12.115.tar.gz",
    sha256: "050fcea96b607e49f135f270472bc95346c172c9d6df5f9256aeb6f7972675d5",
    member: None,
    member_sha256: None,
};

/// A Homebrew bottle file (path relative to `brew --prefix <formula>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HomebrewPin {
    pub formula: &'static str,
    pub file: &'static str,
    pub sha256: &'static str,
}

/// Everything pinned about one library on one platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibPin {
    /// `libkrun` | `libkrunfw`.
    pub name: &'static str,
    pub version: &'static str,
    /// Installed file name, which is also the leaf name libkrun looks libkrunfw up by.
    pub file_name: &'static str,
    pub git: GitPin,
    pub homebrew: Option<HomebrewPin>,
    pub release: Option<AssetPin>,
    /// Digest of the reference from-source build (informational: builds are not
    /// byte-reproducible across toolchains; the enforced pin for `source` is `git.commit`).
    pub reference_build_sha256: Option<&'static str>,
}

/// The pins of one (os, arch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformPins {
    /// `std::env::consts::OS` value.
    pub os: &'static str,
    /// `std::env::consts::ARCH` value.
    pub arch: &'static str,
    pub libkrun: LibPin,
    pub libkrunfw: LibPin,
}

impl PlatformPins {
    fn libs(&self) -> [&LibPin; 2] {
        [&self.libkrun, &self.libkrunfw]
    }
}

/// macOS arm64: Homebrew bottles (`arm64_tahoe`), observed in the 2026-11-24 spike.
pub const MACOS_AARCH64: PlatformPins = PlatformPins {
    os: "macos",
    arch: "aarch64",
    libkrun: LibPin {
        name: "libkrun",
        version: LIBKRUN_VERSION,
        file_name: "libkrun.1.dylib",
        git: LIBKRUN_GIT,
        homebrew: Some(HomebrewPin {
            formula: "libkrun",
            file: "lib/libkrun.1.19.8.dylib",
            sha256: "3c2ffda8f326e6cdcbb504d8e7dc450f84dca8c57b001fce7b6bc6dbdaca5b5a",
        }),
        release: None,
        // TODO(lk-e2e-macos-source): record a reference `--from-source` build digest.
        reference_build_sha256: None,
    },
    libkrunfw: LibPin {
        name: "libkrunfw",
        version: LIBKRUNFW_VERSION,
        file_name: "libkrunfw.5.dylib",
        git: LIBKRUNFW_GIT,
        homebrew: Some(HomebrewPin {
            formula: "libkrunfw",
            file: "lib/libkrunfw.5.dylib",
            sha256: "1ccb5283921f36fedaa8f85ba6265e298a6b86d56be49f642c768b32102b91df",
        }),
        release: None,
        reference_build_sha256: None,
    },
};

/// Linux x86_64 (KVM): libkrun from the pinned tag; libkrunfw from its prebuilt release asset
/// (or from source with `--fw-from-source`, a kernel build).
pub const LINUX_X86_64: PlatformPins = PlatformPins {
    os: "linux",
    arch: "x86_64",
    libkrun: LibPin {
        name: "libkrun",
        version: LIBKRUN_VERSION,
        file_name: "libkrun.so.1",
        git: LIBKRUN_GIT,
        homebrew: None,
        release: None,
        // Reference build on the KVM host (Ubuntu 26.04, stable Rust, `BLK=1 NET=1`), after
        // `patchelf`. (The earlier `BLK=1`-only build was 16a1b77f…; it lacks
        // `krun_add_net_unixstream`.)
        reference_build_sha256: Some(
            "5a302ed1640829d86a73e30a38eb78def99cdf6cec9489c682dbdc2b3493ed69",
        ),
    },
    libkrunfw: LibPin {
        name: "libkrunfw",
        version: LIBKRUNFW_VERSION,
        file_name: "libkrunfw.so.5",
        git: LIBKRUNFW_GIT,
        homebrew: None,
        release: Some(AssetPin {
            url: "https://github.com/libkrun/libkrunfw/releases/download/v5.6.4/libkrunfw-x86_64.tgz",
            sha256: "25a5554137dcc11dd71246627264f67539154ee783aa8450acfa79139e802e64",
            member: Some("lib/x86_64-linux-gnu/libkrunfw.so.5.6.4"),
            member_sha256: Some("5ec9c03e97e2f6df7817b702eb8bfbc090b36c14186b33ddef4a2e9bfccb1c24"),
        }),
        // `--fw-from-source` (kernel build) reference on the KVM host. Differs from the
        // release member: the upstream CI toolchain is not the host's.
        reference_build_sha256: Some(
            "f57aed764d365805de5610ac6d6db99bdc1f04773b57ee187a249116bbfddc55",
        ),
    },
};

/// Pins for an (os, arch), `None` when libkrun is unsupported there.
#[must_use]
pub fn pins_for(os: &str, arch: &str) -> Option<&'static PlatformPins> {
    [&MACOS_AARCH64, &LINUX_X86_64]
        .into_iter()
        .find(|p| p.os == os && p.arch == arch)
}

/// Pins for the running host.
pub fn host_pins() -> Result<&'static PlatformPins, ArtifactError> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    pins_for(os, arch).ok_or_else(|| ArtifactError::Unpinned {
        what: format!("libkrun for {os}/{arch}"),
        hint: "supported: macOS aarch64, Linux x86_64".into(),
    })
}

/// Default artifact dir: `<home>/artifacts/libkrun/<LIBKRUN_VERSION>/`.
#[must_use]
pub fn default_dir(home: &Path) -> PathBuf {
    home.join("artifacts").join("libkrun").join(LIBKRUN_VERSION)
}

// ---------------------------------------------------------------------------------------------
// Errors

/// Artifact resolution / install failures. Converts into [`sandcrab_core::SandboxError`].
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("unpinned {what}: refusing to use it ({hint})")]
    Unpinned { what: String, hint: String },
    #[error(
        "libkrun artifacts not installed at `{dir}`: {detail}; run `scripts/fetch-libkrun.sh` \
         (or `sandcrab libkrun install`)"
    )]
    NotInstalled { dir: PathBuf, detail: String },
    #[error("libkrun manifest `{path}` does not match the pins of this build: {detail}; reinstall")]
    ManifestMismatch { path: PathBuf, detail: String },
    #[error("digest mismatch for `{path}`: expected {expected}, got {actual}")]
    DigestMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("`{cmd}` failed: {detail}")]
    Command { cmd: String, detail: String },
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl From<ArtifactError> for sandcrab_core::SandboxError {
    fn from(e: ArtifactError) -> Self {
        match e {
            ArtifactError::DigestMismatch {
                path,
                expected,
                actual,
            } => Self::DigestMismatch {
                image: path.display().to_string(),
                expected,
                actual,
            },
            other => Self::Backend {
                backend: "libkrun".into(),
                message: other.to_string(),
            },
        }
    }
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> ArtifactError {
    let context = context.into();
    move |source| ArtifactError::Io { context, source }
}

// ---------------------------------------------------------------------------------------------
// Hashing

/// Lowercase hex SHA-256 of a file (streamed).
pub fn sha256_file(path: &Path) -> Result<String, ArtifactError> {
    let mut f = fs::File::open(path).map_err(io_err(format!("open {}", path.display())))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(io_err(format!("read {}", path.display())))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn verify_sha256(path: &Path, expected: &str) -> Result<(), ArtifactError> {
    let actual = sha256_file(path)?;
    if actual == expected {
        Ok(())
    } else {
        Err(ArtifactError::DigestMismatch {
            path: path.to_path_buf(),
            expected: expected.to_string(),
            actual,
        })
    }
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

// ---------------------------------------------------------------------------------------------
// Manifest

/// Where an installed library came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Origin {
    /// Copied from the Homebrew bottle (`origin_sha256` = bottle file sha).
    Homebrew,
    /// Extracted from a pinned upstream release asset (`origin_sha256` = member sha).
    Release,
    /// Built from the pinned git tag (`git_commit`).
    Source,
}

impl Origin {
    /// The manifest spelling (`homebrew` | `release` | `source`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Homebrew => "homebrew",
            Self::Release => "release",
            Self::Source => "source",
        }
    }
}

/// One library entry of `manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibManifest {
    pub version: String,
    pub file: String,
    pub origin: Origin,
    /// Digest of the pinned input file (bottle file / release member); `None` for `source`.
    pub origin_sha256: Option<String>,
    /// Commit the source build was made from; `None` unless `origin = source`.
    pub git_commit: Option<String>,
    /// Digest of the installed file.
    pub sha256: String,
    /// Post-copy modifications, in order (e.g. `install_name_tool -id libkrunfw.5.dylib`).
    pub fixups: Vec<String>,
}

/// `manifest.json` (written last, after every file is in place).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub os: String,
    pub arch: String,
    pub libkrun: LibManifest,
    pub libkrunfw: LibManifest,
}

impl Manifest {
    /// Read and parse `<dir>/manifest.json`.
    pub fn load(dir: &Path) -> Result<Self, ArtifactError> {
        let path = dir.join(MANIFEST_NAME);
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(ArtifactError::NotInstalled {
                    dir: dir.to_path_buf(),
                    detail: format!("no {MANIFEST_NAME}"),
                });
            }
            Err(e) => return Err(io_err(format!("read {}", path.display()))(e)),
        };
        serde_json::from_slice(&raw).map_err(|e| ArtifactError::ManifestMismatch {
            path,
            detail: format!("unparseable: {e}"),
        })
    }

    /// Check every manifest field against the compiled-in pins (no file I/O).
    pub fn check(&self, pins: &PlatformPins) -> Result<(), String> {
        if self.schema != MANIFEST_SCHEMA {
            return Err(format!("schema {} (want {MANIFEST_SCHEMA})", self.schema));
        }
        if self.os != pins.os || self.arch != pins.arch {
            return Err(format!(
                "installed for {}/{}, this host is {}/{}",
                self.os, self.arch, pins.os, pins.arch
            ));
        }
        check_lib(pins.libkrun, &self.libkrun)?;
        check_lib(pins.libkrunfw, &self.libkrunfw)
    }
}

fn check_lib(pin: LibPin, m: &LibManifest) -> Result<(), String> {
    let name = pin.name;
    if m.version != pin.version {
        return Err(format!(
            "{name} {} installed, {} pinned",
            m.version, pin.version
        ));
    }
    if m.file != pin.file_name {
        return Err(format!(
            "{name} file `{}`, expected `{}`",
            m.file, pin.file_name
        ));
    }
    if !is_sha256_hex(&m.sha256) {
        return Err(format!("{name} sha256 `{}` is malformed", m.sha256));
    }
    let origin_sha = || m.origin_sha256.as_deref().unwrap_or("<none>");
    match m.origin {
        Origin::Homebrew | Origin::Release => {
            let pinned = match m.origin {
                Origin::Homebrew => pin.homebrew.map(|h| h.sha256),
                _ => pin.release.and_then(|r| r.member_sha256),
            };
            let Some(pinned) = pinned else {
                return Err(format!(
                    "{name}: no pinned {} artifact on this platform",
                    m.origin.as_str()
                ));
            };
            if m.origin_sha256.as_deref() != Some(pinned) {
                return Err(format!(
                    "{name} {} input sha {} is not the pinned {pinned}",
                    m.origin.as_str(),
                    origin_sha()
                ));
            }
            if m.fixups.is_empty() && m.sha256 != pinned {
                return Err(format!(
                    "{name} installed sha {} differs from its unmodified input {pinned}",
                    m.sha256
                ));
            }
        }
        Origin::Source => {
            if m.git_commit.as_deref() != Some(pin.git.commit) {
                return Err(format!(
                    "{name} built from commit {}, pinned {} ({})",
                    m.git_commit.as_deref().unwrap_or("<none>"),
                    pin.git.commit,
                    pin.git.tag
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Resolution

/// A library path plus the SHA-256 it must have when loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedPath {
    pub path: PathBuf,
    pub sha256: String,
}

impl PinnedPath {
    /// Re-hash the file and compare (call right before `dlopen`).
    pub fn verify(&self) -> Result<(), ArtifactError> {
        verify_sha256(&self.path, &self.sha256)
    }
}

/// The verified libkrun + libkrunfw to load (libkrunfw first, see the module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedArtifacts {
    pub dir: PathBuf,
    pub libkrun: PinnedPath,
    pub libkrunfw: PinnedPath,
    /// `None` when resolved with `verify_pins = false` (no manifest/pin checks).
    pub manifest: Option<Manifest>,
}

/// Namespace for artifact resolution.
pub struct Artifacts;

impl Artifacts {
    /// Resolve the libraries for `config`: `config.lib_dir`, else
    /// `<sandcrab_home>/artifacts/libkrun/<LIBKRUN_VERSION>/`; never a system path.
    pub fn resolve(config: &LibkrunConfig) -> Result<ResolvedArtifacts, ArtifactError> {
        let dir = match &config.lib_dir {
            Some(d) => d.clone(),
            None => default_dir(&sandcrab_rootfs::sandcrab_home().map_err(|e| {
                ArtifactError::NotInstalled {
                    dir: PathBuf::from("~/.sandcrab"),
                    detail: e.to_string(),
                }
            })?),
        };
        Self::resolve_dir(&dir, config.verify_pins)
    }

    /// Resolve the libraries in `dir` against the host's pins.
    pub fn resolve_dir(dir: &Path, verify_pins: bool) -> Result<ResolvedArtifacts, ArtifactError> {
        Self::resolve_with(dir, host_pins()?, verify_pins)
    }

    /// [`Self::resolve_dir`] against explicit pins (tests, `doctor` for other platforms).
    pub fn resolve_with(
        dir: &Path,
        pins: &PlatformPins,
        verify_pins: bool,
    ) -> Result<ResolvedArtifacts, ArtifactError> {
        if !verify_pins {
            let unverified = |pin: &LibPin| -> Result<PinnedPath, ArtifactError> {
                let path = dir.join(pin.file_name);
                if !path.is_file() {
                    return Err(ArtifactError::NotInstalled {
                        dir: dir.to_path_buf(),
                        detail: format!("missing {}", pin.file_name),
                    });
                }
                let sha256 = sha256_file(&path)?;
                Ok(PinnedPath { path, sha256 })
            };
            tracing::warn!(dir = %dir.display(), "libkrun pin verification disabled (verify_pins=false)");
            return Ok(ResolvedArtifacts {
                dir: dir.to_path_buf(),
                libkrun: unverified(&pins.libkrun)?,
                libkrunfw: unverified(&pins.libkrunfw)?,
                manifest: None,
            });
        }
        let manifest = Manifest::load(dir)?;
        manifest
            .check(pins)
            .map_err(|detail| ArtifactError::ManifestMismatch {
                path: dir.join(MANIFEST_NAME),
                detail,
            })?;
        let pinned = |m: &LibManifest| -> Result<PinnedPath, ArtifactError> {
            let p = PinnedPath {
                path: dir.join(&m.file),
                sha256: m.sha256.clone(),
            };
            if !p.path.is_file() {
                return Err(ArtifactError::NotInstalled {
                    dir: dir.to_path_buf(),
                    detail: format!("missing {}", m.file),
                });
            }
            p.verify()?;
            Ok(p)
        };
        Ok(ResolvedArtifacts {
            dir: dir.to_path_buf(),
            libkrun: pinned(&manifest.libkrun)?,
            libkrunfw: pinned(&manifest.libkrunfw)?,
            manifest: Some(manifest),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Install (support for `sandcrab libkrun install`; `scripts/fetch-libkrun.sh` mirrors it)

/// One library to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub path: PathBuf,
    pub origin: Origin,
    /// Required for [`Origin::Source`]: the commit the file was built from.
    pub git_commit: Option<String>,
}

/// The two libraries to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallInput {
    pub libkrun: SourceFile,
    pub libkrunfw: SourceFile,
}

/// A post-copy modification of an installed library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fixup {
    /// macOS: `install_name_tool -id <leaf>` then `codesign -f -s -` (ad hoc re-sign).
    MachoLeafId(String),
    /// Linux: `patchelf --set-rpath '$ORIGIN'`.
    ElfRunpathOrigin,
}

impl Fixup {
    /// Manifest wording (the script writes the same strings).
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        match self {
            Self::MachoLeafId(leaf) => vec![
                format!("install_name_tool -id {leaf}"),
                "codesign -f -s -".into(),
            ],
            Self::ElfRunpathOrigin => vec!["patchelf --set-rpath $ORIGIN".into()],
        }
    }

    fn apply(&self, path: &Path) -> Result<(), ArtifactError> {
        match self {
            Self::MachoLeafId(leaf) => {
                // Absolute paths: never pick up a non-Apple `install_name_tool` from PATH.
                run(Command::new("/usr/bin/install_name_tool")
                    .args(["-id", leaf])
                    .arg(path))?;
                run(Command::new("/usr/bin/codesign")
                    .args(["--force", "--sign", "-"])
                    .arg(path))
            }
            Self::ElfRunpathOrigin => run(Command::new("patchelf")
                .args(["--set-rpath", "$ORIGIN"])
                .arg(path)),
        }
    }
}

/// The fix-ups the installer applies to `pin` from `origin` on `pins.os` (see the module doc).
#[must_use]
pub fn fixups_for(pins: &PlatformPins, pin: &LibPin, origin: Origin) -> Vec<Fixup> {
    match (pins.os, pin.name, origin) {
        ("macos", "libkrunfw", _) => vec![Fixup::MachoLeafId(pin.file_name.into())],
        ("linux", "libkrun", Origin::Source) => vec![Fixup::ElfRunpathOrigin],
        _ => Vec::new(),
    }
}

fn run(cmd: &mut Command) -> Result<(), ArtifactError> {
    let shown = format!("{cmd:?}");
    let out = cmd.output().map_err(|e| ArtifactError::Command {
        cmd: shown.clone(),
        detail: e.to_string(),
    })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ArtifactError::Command {
            cmd: shown,
            detail: format!(
                "{}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        })
    }
}

/// Install `input` into `default_dir(home)` for the host's pins: verify each input's provenance,
/// copy, fix up, record the manifest, swap the directory into place. Returns the resolved result.
pub fn install_files(
    home: &Path,
    input: &InstallInput,
) -> Result<ResolvedArtifacts, ArtifactError> {
    let pins = host_pins()?;
    install_with(&default_dir(home), pins, input, &|f, p| f.apply(p))
}

/// Copy the pinned Homebrew bottles (`brew --prefix libkrun|libkrunfw`) into
/// `default_dir(home)` (macOS arm64; the §18.2 default when `brew` is present).
pub fn install_homebrew(home: &Path) -> Result<ResolvedArtifacts, ArtifactError> {
    let pins = host_pins()?;
    let mut files = Vec::new();
    for pin in pins.libs() {
        let hb = pin.homebrew.ok_or_else(|| ArtifactError::Unpinned {
            what: format!("Homebrew {} on {}/{}", pin.name, pins.os, pins.arch),
            hint: "use scripts/fetch-libkrun.sh --from-source".into(),
        })?;
        let prefix = brew_prefix(hb.formula)?;
        files.push(SourceFile {
            path: prefix.join(hb.file),
            origin: Origin::Homebrew,
            git_commit: None,
        });
    }
    let [libkrun, libkrunfw]: [SourceFile; 2] = files.try_into().expect("two libs");
    install_files(home, &InstallInput { libkrun, libkrunfw })
}

fn brew_prefix(formula: &str) -> Result<PathBuf, ArtifactError> {
    let mut cmd = Command::new("brew");
    cmd.args(["--prefix", formula]);
    let shown = format!("{cmd:?}");
    let out = cmd.output().map_err(|e| ArtifactError::Command {
        cmd: shown.clone(),
        detail: format!("{e} (Homebrew not found; use scripts/fetch-libkrun.sh --from-source)"),
    })?;
    if !out.status.success() {
        return Err(ArtifactError::Command {
            cmd: shown,
            detail: String::from_utf8_lossy(&out.stderr).trim().into(),
        });
    }
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if !p.is_dir() {
        return Err(ArtifactError::NotInstalled {
            dir: p,
            detail: format!("run `brew install {formula}`"),
        });
    }
    Ok(p)
}

type ApplyFixup<'a> = dyn Fn(&Fixup, &Path) -> Result<(), ArtifactError> + 'a;

fn install_with(
    dest: &Path,
    pins: &PlatformPins,
    input: &InstallInput,
    apply: &ApplyFixup<'_>,
) -> Result<ResolvedArtifacts, ArtifactError> {
    let parent = dest.parent().expect("artifact dir has a parent");
    fs::create_dir_all(parent).map_err(io_err(format!("create {}", parent.display())))?;
    let leaf = dest.file_name().expect("versioned dir").to_string_lossy();
    let staging = parent.join(format!(".{leaf}.staging.{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    let res = (|| {
        fs::create_dir(&staging).map_err(io_err(format!("create {}", staging.display())))?;
        let libkrun = stage_lib(&staging, pins, &pins.libkrun, &input.libkrun, apply)?;
        let libkrunfw = stage_lib(&staging, pins, &pins.libkrunfw, &input.libkrunfw, apply)?;
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA,
            os: pins.os.into(),
            arch: pins.arch.into(),
            libkrun,
            libkrunfw,
        };
        manifest
            .check(pins)
            .map_err(|detail| ArtifactError::ManifestMismatch {
                path: staging.join(MANIFEST_NAME),
                detail,
            })?;
        let mpath = staging.join(MANIFEST_NAME);
        let mut json = serde_json::to_vec_pretty(&manifest).expect("manifest serializes");
        json.push(b'\n');
        let mut f =
            fs::File::create(&mpath).map_err(io_err(format!("create {}", mpath.display())))?;
        f.write_all(&json)
            .and_then(|()| f.sync_all())
            .map_err(io_err(format!("write {}", mpath.display())))?;
        // Swap into place: old → .old, staging → dest, drop old.
        let old = parent.join(format!(".{leaf}.old.{}", std::process::id()));
        if dest.exists() {
            fs::rename(dest, &old).map_err(io_err(format!("move aside {}", dest.display())))?;
        }
        fs::rename(&staging, dest).map_err(io_err(format!("install {}", dest.display())))?;
        let _ = fs::remove_dir_all(&old);
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    res?;
    Artifacts::resolve_with(dest, pins, true)
}

fn stage_lib(
    staging: &Path,
    pins: &PlatformPins,
    pin: &LibPin,
    src: &SourceFile,
    apply: &ApplyFixup<'_>,
) -> Result<LibManifest, ArtifactError> {
    let unpinned = |what: &str| ArtifactError::Unpinned {
        what: format!("{what} {} on {}/{}", pin.name, pins.os, pins.arch),
        hint: "this build has no pin for that source".into(),
    };
    let (origin_sha256, git_commit) = match src.origin {
        Origin::Homebrew => {
            let hb = pin.homebrew.ok_or_else(|| unpinned("Homebrew"))?;
            verify_sha256(&src.path, hb.sha256)?;
            (Some(hb.sha256.to_string()), None)
        }
        Origin::Release => {
            let sha = pin
                .release
                .and_then(|r| r.member_sha256)
                .ok_or_else(|| unpinned("release"))?;
            verify_sha256(&src.path, sha)?;
            (Some(sha.to_string()), None)
        }
        Origin::Source => {
            if src.git_commit.as_deref() != Some(pin.git.commit) {
                return Err(unpinned(&format!(
                    "source build from commit {}",
                    src.git_commit.as_deref().unwrap_or("<unknown>")
                )));
            }
            (None, Some(pin.git.commit.to_string()))
        }
    };
    let dest = staging.join(pin.file_name);
    fs::copy(&src.path, &dest).map_err(io_err(format!(
        "copy {} → {}",
        src.path.display(),
        dest.display()
    )))?;
    let set_mode = |mode| {
        fs::set_permissions(&dest, fs::Permissions::from_mode(mode))
            .map_err(io_err(format!("chmod {}", dest.display())))
    };
    set_mode(0o644)?;
    let fixups = fixups_for(pins, pin, src.origin);
    for f in &fixups {
        apply(f, &dest)?;
    }
    set_mode(0o444)?;
    Ok(LibManifest {
        version: pin.version.into(),
        file: pin.file_name.into(),
        origin: src.origin,
        origin_sha256,
        git_commit,
        sha256: sha256_file(&dest)?,
        fixups: fixups.iter().flat_map(Fixup::describe).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const SHA_XYZ: &str = "3608bca1e44ea6c4d268eb6db02260269892c0b42b86bbf1e77a6fa16c3c9282";

    /// Test pins: a "linux" platform whose libkrunfw comes from a release (no fix-up) and whose
    /// libkrun is a source build (patchelf), plus Homebrew pins on libkrun.
    fn test_pins() -> PlatformPins {
        let mut p = LINUX_X86_64;
        p.libkrun.homebrew = Some(HomebrewPin {
            formula: "libkrun",
            file: "lib/x",
            sha256: SHA_XYZ,
        });
        p.libkrunfw.release = Some(AssetPin {
            url: "https://example.invalid/fw.tgz",
            sha256: SHA_ABC,
            member: Some("fw"),
            member_sha256: Some(SHA_ABC),
        });
        p
    }

    /// Fix-up stand-in: append a byte (changes the digest like a real one would).
    fn fake_fixup(_: &Fixup, p: &Path) -> Result<(), ArtifactError> {
        let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
        f.write_all(b"!").unwrap();
        Ok(())
    }

    fn inputs(dir: &Path) -> InstallInput {
        fs::write(dir.join("krun-src"), b"built").unwrap();
        fs::write(dir.join("fw-src"), b"abc").unwrap();
        InstallInput {
            libkrun: SourceFile {
                path: dir.join("krun-src"),
                origin: Origin::Source,
                git_commit: Some(LIBKRUN_GIT.commit.into()),
            },
            libkrunfw: SourceFile {
                path: dir.join("fw-src"),
                origin: Origin::Release,
                git_commit: None,
            },
        }
    }

    #[test]
    fn pins_are_well_formed() {
        for p in [&MACOS_AARCH64, &LINUX_X86_64] {
            assert_eq!(pins_for(p.os, p.arch), Some(p));
            for lib in p.libs() {
                assert!(lib.git.repo.starts_with("https://github.com/libkrun/"));
                assert_eq!(lib.git.tag, format!("v{}", lib.version));
                assert_eq!(lib.git.commit.len(), 40);
                let mut shas: Vec<&str> = Vec::new();
                shas.extend(lib.homebrew.map(|h| h.sha256));
                if let Some(r) = lib.release {
                    assert!(r.url.starts_with("https://") && r.url.contains(lib.version));
                    shas.push(r.sha256);
                    shas.extend(r.member_sha256);
                    assert_eq!(r.member.is_some(), r.member_sha256.is_some());
                }
                shas.extend(lib.reference_build_sha256);
                for s in shas {
                    assert!(is_sha256_hex(s), "{s}");
                }
            }
        }
        assert!(is_sha256_hex(LIBKRUNFW_PREBUILT_KERNEL_AARCH64.sha256));
        assert!(is_sha256_hex(LIBKRUNFW_KERNEL_SOURCE.sha256));
        assert!(LIBKRUNFW_KERNEL_SOURCE.url.contains(LIBKRUNFW_VERSION));
        assert!(pins_for("macos", "x86_64").is_none());
        assert!(pins_for("linux", "aarch64").is_none());
        // Homebrew pins exist only on macOS; libkrun has no release assets upstream.
        assert!(LINUX_X86_64.libkrun.homebrew.is_none() && LINUX_X86_64.libkrun.release.is_none());
        assert!(MACOS_AARCH64.libkrunfw.homebrew.is_some());
        assert!(LINUX_X86_64.libkrunfw.release.is_some());
    }

    #[test]
    fn fixups_per_platform() {
        let m = &MACOS_AARCH64;
        assert_eq!(fixups_for(m, &m.libkrun, Origin::Homebrew), vec![]);
        assert_eq!(
            fixups_for(m, &m.libkrunfw, Origin::Homebrew),
            vec![Fixup::MachoLeafId("libkrunfw.5.dylib".into())]
        );
        let l = &LINUX_X86_64;
        assert_eq!(
            fixups_for(l, &l.libkrun, Origin::Source),
            vec![Fixup::ElfRunpathOrigin]
        );
        assert_eq!(fixups_for(l, &l.libkrunfw, Origin::Release), vec![]);
        assert_eq!(fixups_for(l, &l.libkrunfw, Origin::Source), vec![]);
    }

    #[test]
    fn install_then_resolve() {
        let t = tempfile::tempdir().unwrap();
        let pins = test_pins();
        let dest = t.path().join("a/libkrun/1.19.8");
        let r = install_with(&dest, &pins, &inputs(t.path()), &fake_fixup).unwrap();
        assert_eq!(r.libkrunfw.path, dest.join("libkrunfw.so.5"));
        assert_eq!(
            r.libkrunfw.sha256, SHA_ABC,
            "release member copied unmodified"
        );
        assert_eq!(fs::read(&r.libkrun.path).unwrap(), b"built!");
        assert_eq!(r.libkrun.sha256, sha256_file(&r.libkrun.path).unwrap());
        let m = r.manifest.as_ref().unwrap();
        assert_eq!(m.libkrun.origin, Origin::Source);
        assert_eq!(m.libkrun.fixups, vec!["patchelf --set-rpath $ORIGIN"]);
        assert!(m.libkrunfw.fixups.is_empty());
        let mode = fs::metadata(&r.libkrun.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o444);
        // Re-resolving gives the same answer; reinstalling over it works.
        assert_eq!(Artifacts::resolve_with(&dest, &pins, true).unwrap(), r);
        install_with(&dest, &pins, &inputs(t.path()), &fake_fixup).unwrap();
        // No staging / old dirs left behind.
        let left: Vec<_> = fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("1.19.8")]);
    }

    #[test]
    fn tampered_file_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let pins = test_pins();
        let dest = t.path().join("d");
        let r = install_with(&dest, &pins, &inputs(t.path()), &fake_fixup).unwrap();
        fs::set_permissions(&r.libkrunfw.path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&r.libkrunfw.path, b"evil").unwrap();
        let e = Artifacts::resolve_with(&dest, &pins, true).unwrap_err();
        assert!(matches!(e, ArtifactError::DigestMismatch { .. }), "{e}");
        let core: sandcrab_core::SandboxError = e.into();
        assert!(matches!(
            core,
            sandcrab_core::SandboxError::DigestMismatch { .. }
        ));
        // verify_pins=false accepts it, and the returned sha is the current digest.
        let u = Artifacts::resolve_with(&dest, &pins, false).unwrap();
        assert!(u.manifest.is_none());
        u.libkrunfw.verify().unwrap();
        // PinnedPath::verify catches a swap after resolution.
        fs::write(&r.libkrunfw.path, b"evil2").unwrap();
        assert!(u.libkrunfw.verify().is_err());
    }

    #[test]
    fn wrong_inputs_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let pins = test_pins();
        let dest = t.path().join("d");
        // Release input with the wrong digest.
        let i = inputs(t.path());
        fs::write(&i.libkrunfw.path, b"not abc").unwrap();
        let e = install_with(&dest, &pins, &i, &fake_fixup).unwrap_err();
        assert!(matches!(e, ArtifactError::DigestMismatch { .. }), "{e}");
        assert!(!dest.exists());
        // Source build from another commit.
        let mut i = inputs(t.path());
        i.libkrun.git_commit = Some("0".repeat(40));
        let e = install_with(&dest, &pins, &i, &fake_fixup).unwrap_err();
        assert!(matches!(e, ArtifactError::Unpinned { .. }), "{e}");
        // An origin this platform has no pin for (Homebrew libkrunfw on Linux).
        let mut i = inputs(t.path());
        i.libkrunfw.origin = Origin::Homebrew;
        let e = install_with(&dest, &pins, &i, &fake_fixup).unwrap_err();
        assert!(e.to_string().contains("unpinned Homebrew libkrunfw"), "{e}");
        assert!(!dest.exists(), "nothing installed on failure");
        assert_eq!(
            fs::read_dir(t.path()).unwrap().count(),
            2,
            "no staging left"
        );
    }

    #[test]
    fn manifest_checks() {
        let t = tempfile::tempdir().unwrap();
        let pins = test_pins();
        let dest = t.path().join("d");
        let good = install_with(&dest, &pins, &inputs(t.path()), &fake_fixup)
            .unwrap()
            .manifest
            .unwrap();
        good.check(&pins).unwrap();
        let bad = |f: &dyn Fn(&mut Manifest)| {
            let mut m = good.clone();
            f(&mut m);
            m.check(&pins).unwrap_err()
        };
        assert!(bad(&|m| m.os = "macos".into()).contains("installed for macos"));
        assert!(bad(&|m| m.libkrun.version = "1.19.7".into()).contains("1.19.7 installed"));
        assert!(bad(&|m| m.libkrunfw.file = "libkrunfw.so".into()).contains("file"));
        assert!(bad(&|m| m.libkrun.git_commit = None).contains("built from commit <none>"));
        assert!(
            bad(&|m| m.libkrunfw.origin_sha256 = Some(SHA_XYZ.into())).contains("not the pinned")
        );
        // An unmodified release input must be installed byte for byte.
        assert!(bad(&|m| m.libkrunfw.sha256 = SHA_XYZ.into()).contains("unmodified"));
        assert!(bad(&|m| m.libkrun.sha256 = "ABC".into()).contains("malformed"));
        assert!(bad(&|m| m.schema = 2).contains("schema"));
        // Homebrew origin is checked against the Homebrew pin.
        let mut m = good.clone();
        m.libkrun.origin = Origin::Homebrew;
        m.libkrun.origin_sha256 = Some(SHA_XYZ.into());
        m.check(&pins).unwrap();
        m.libkrun.origin_sha256 = Some(SHA_ABC.into());
        assert!(m.check(&pins).is_err());
        // Real pins: Homebrew libkrunfw is unpinned on Linux.
        let mut m = good.clone();
        m.libkrunfw.origin = Origin::Homebrew;
        assert!(m.check(&LINUX_X86_64).unwrap_err().contains("no pinned"));
    }

    #[test]
    fn missing_install_is_a_clear_error() {
        let t = tempfile::tempdir().unwrap();
        let e = Artifacts::resolve_with(t.path(), &test_pins(), true).unwrap_err();
        assert!(e.to_string().contains("scripts/fetch-libkrun.sh"), "{e}");
        fs::write(t.path().join(MANIFEST_NAME), b"{").unwrap();
        let e = Artifacts::resolve_with(t.path(), &test_pins(), true).unwrap_err();
        assert!(matches!(e, ArtifactError::ManifestMismatch { .. }), "{e}");
        let e = Artifacts::resolve_with(t.path(), &test_pins(), false).unwrap_err();
        assert!(e.to_string().contains("missing libkrun.so.1"), "{e}");
    }

    #[test]
    fn resolve_uses_lib_dir_override() {
        let t = tempfile::tempdir().unwrap();
        let mut c = LibkrunConfig::with_home(Path::new("/nonexistent-home"), "/c");
        c.lib_dir = Some(t.path().to_path_buf());
        let e = Artifacts::resolve(&c).unwrap_err().to_string();
        if host_pins().is_ok() {
            assert!(e.contains(&t.path().display().to_string()), "{e}");
        } else {
            assert!(e.contains("unpinned"), "{e}");
        }
    }

    /// `scripts/fetch-libkrun.sh` mirrors these pins; keep them in lockstep.
    #[test]
    fn script_mirrors_pins() {
        let script = include_str!("../../../scripts/fetch-libkrun.sh");
        let get = |key: &str| -> String {
            let line = script
                .lines()
                .find(|l| l.starts_with(&format!("{key}=")))
                .unwrap_or_else(|| panic!("{key}= missing in fetch-libkrun.sh"));
            line[key.len() + 1..].trim_matches('"').to_string()
        };
        let m = &MACOS_AARCH64;
        let l = &LINUX_X86_64;
        let fw_rel = l.libkrunfw.release.unwrap();
        for (key, want) in [
            ("LIBKRUN_VERSION", LIBKRUN_VERSION),
            ("LIBKRUNFW_VERSION", LIBKRUNFW_VERSION),
            ("LIBKRUN_REPO", LIBKRUN_GIT.repo),
            ("LIBKRUN_TAG", LIBKRUN_GIT.tag),
            ("LIBKRUN_COMMIT", LIBKRUN_GIT.commit),
            ("LIBKRUNFW_REPO", LIBKRUNFW_GIT.repo),
            ("LIBKRUNFW_TAG", LIBKRUNFW_GIT.tag),
            ("LIBKRUNFW_COMMIT", LIBKRUNFW_GIT.commit),
            ("MAC_LIBKRUN_BOTTLE_FILE", m.libkrun.homebrew.unwrap().file),
            (
                "MAC_LIBKRUN_BOTTLE_SHA256",
                m.libkrun.homebrew.unwrap().sha256,
            ),
            (
                "MAC_LIBKRUNFW_BOTTLE_FILE",
                m.libkrunfw.homebrew.unwrap().file,
            ),
            (
                "MAC_LIBKRUNFW_BOTTLE_SHA256",
                m.libkrunfw.homebrew.unwrap().sha256,
            ),
            ("MAC_LIBKRUN_FILE", m.libkrun.file_name),
            ("MAC_LIBKRUNFW_FILE", m.libkrunfw.file_name),
            ("MAC_FW_KERNEL_URL", LIBKRUNFW_PREBUILT_KERNEL_AARCH64.url),
            (
                "MAC_FW_KERNEL_SHA256",
                LIBKRUNFW_PREBUILT_KERNEL_AARCH64.sha256,
            ),
            ("FW_KERNEL_SRC_URL", LIBKRUNFW_KERNEL_SOURCE.url),
            ("FW_KERNEL_SRC_SHA256", LIBKRUNFW_KERNEL_SOURCE.sha256),
            ("LINUX_LIBKRUN_FILE", l.libkrun.file_name),
            ("LINUX_LIBKRUNFW_FILE", l.libkrunfw.file_name),
            ("LINUX_FW_URL", fw_rel.url),
            ("LINUX_FW_SHA256", fw_rel.sha256),
            ("LINUX_FW_MEMBER", fw_rel.member.unwrap()),
            ("LINUX_FW_MEMBER_SHA256", fw_rel.member_sha256.unwrap()),
            ("MANIFEST_SCHEMA", &MANIFEST_SCHEMA.to_string()),
        ] {
            assert_eq!(get(key), want, "{key}");
        }
    }

    /// End to end on a Mac with `brew install libkrun libkrunfw` at the pinned versions:
    /// the Rust installer and `scripts/fetch-libkrun.sh` produce the same manifest, both
    /// resolve, and the script refuses a tampered bottle (fake `brew` shim).
    #[test]
    #[ignore = "needs macOS arm64 + Homebrew libkrun/libkrunfw at the pinned versions"]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn homebrew_install_matches_script() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fetch-libkrun.sh");
        let t = tempfile::tempdir().unwrap();
        let rust_home = t.path().join("rust");
        let r = install_homebrew(&rust_home).unwrap();
        let rm = r.manifest.clone().unwrap();
        assert_eq!(
            rm.libkrun.sha256,
            MACOS_AARCH64.libkrun.homebrew.unwrap().sha256
        );
        assert_ne!(
            rm.libkrunfw.sha256,
            MACOS_AARCH64.libkrunfw.homebrew.unwrap().sha256
        );

        let sh_home = t.path().join("sh");
        let out = Command::new("bash")
            .arg(&script)
            .arg("--home")
            .arg(&sh_home)
            .arg("--no-netstack")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let s = Artifacts::resolve_dir(&default_dir(&sh_home), true).unwrap();
        assert_eq!(s.manifest.unwrap(), rm, "script and Rust installs agree");

        // Tampered bottle behind a fake `brew`: the script must refuse and install nothing.
        let fake = t.path().join("fake");
        fs::create_dir_all(fake.join("bin")).unwrap();
        for (f, src) in [
            ("libkrun", &r.libkrun.path),
            ("libkrunfw", &r.libkrunfw.path),
        ] {
            let hb = if f == "libkrun" {
                MACOS_AARCH64.libkrun.homebrew.unwrap()
            } else {
                MACOS_AARCH64.libkrunfw.homebrew.unwrap()
            };
            let p = fake.join(f).join(hb.file);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::copy(src, &p).unwrap();
        }
        let shim = fake.join("bin/brew");
        fs::write(
            &shim,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  list) [ \"$3\" = libkrun ] && echo \"libkrun {LIBKRUN_VERSION}\" || echo \"libkrunfw {LIBKRUNFW_VERSION}\" ;;\n  --prefix) echo \"{}/$2\" ;;\nesac\n",
                fake.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        let bad_home = t.path().join("bad");
        let out = Command::new("bash")
            .arg(&script)
            .arg("--home")
            .arg(&bad_home)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    fake.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success());
        assert!(err.contains("SHA-256 mismatch"), "{err}");
        assert!(!default_dir(&bad_home).exists());
        // And the Rust resolver refuses a tampered install.
        let fw = default_dir(&sh_home).join(MACOS_AARCH64.libkrunfw.file_name);
        fs::set_permissions(&fw, fs::Permissions::from_mode(0o644)).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&fw)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        let e = Artifacts::resolve_dir(&default_dir(&sh_home), true).unwrap_err();
        assert!(matches!(e, ArtifactError::DigestMismatch { .. }), "{e}");
    }
}
