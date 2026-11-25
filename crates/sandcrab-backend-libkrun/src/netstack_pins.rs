//! Pinned userspace netstacks (design §9 Phase B): passt on Linux x86_64, gvproxy on macOS
//! arm64. Same provenance model as libkrun ([`crate::artifacts`]):
//!
//! ```text
//! <SANDCRAB_HOME>/artifacts/libkrun-net/<kind>-<version>/
//!   manifest.json     # NetstackManifest: installed sha bound to its pinned provenance
//!   gvproxy | passt   # 0555
//! ```
//!
//! - **gvproxy** (macOS): the upstream release asset `gvproxy-darwin` (universal, ad hoc
//!   linker-signed), installed byte for byte; its SHA-256 is compiled in and is the installed
//!   file's sha.
//! - **passt** (Linux): built by `scripts/fetch-libkrun.sh` from the pinned git tag (commit
//!   checked) as a static binary (`make static`); not byte-reproducible across toolchains, so
//!   the installed sha is recorded in the manifest next to the commit (like libkrun on Linux).
//!
//! [`resolve`] accepts an install only when the manifest matches the compiled-in pin and the
//! file hashes to the manifest's `sha256`. "Not installed" is `Ok(None)` (network
//! `unrestricted` is then `PolicyUnenforceable`); anything else is an error.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::artifacts::{ArtifactError, GitPin, Origin, sha256_file};
use crate::netstack::NetstackKind;

/// Directory under `<home>/artifacts/` holding the netstack installs.
pub const NETSTACK_ARTIFACT_DIR: &str = "libkrun-net";
/// Manifest file name.
pub const NETSTACK_MANIFEST: &str = "manifest.json";
/// Netstack manifest schema version.
pub const NETSTACK_MANIFEST_SCHEMA: u32 = 1;

/// One pinned netstack on one platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetstackPin {
    pub os: &'static str,
    pub arch: &'static str,
    pub kind: NetstackKind,
    pub version: &'static str,
    /// Installed file name.
    pub file_name: &'static str,
    pub git: GitPin,
    /// Upstream release asset (URL, SHA-256), installed unmodified. `None`: source build.
    pub release: Option<(&'static str, &'static str)>,
    /// Reference build digest (informational; source builds are not reproducible).
    pub reference_build_sha256: Option<&'static str>,
}

/// gvproxy v0.9.0 (gvisor-tap-vsock), release asset `gvproxy-darwin`.
pub const GVPROXY_MACOS_AARCH64: NetstackPin = NetstackPin {
    os: "macos",
    arch: "aarch64",
    kind: NetstackKind::Gvproxy,
    version: "0.9.0",
    file_name: "gvproxy",
    git: GitPin {
        repo: "https://github.com/containers/gvisor-tap-vsock",
        tag: "v0.9.0",
        commit: "6dbb5d4fa0033356c72a201346f8350594229868",
    },
    release: Some((
        "https://github.com/containers/gvisor-tap-vsock/releases/download/v0.9.0/gvproxy-darwin",
        "e2a6825ba789fe588828b418c09aaf16525dec6d8cb2d16c94e72535dd4606be",
    )),
    reference_build_sha256: None,
};

/// passt `2026_10_02.cba3570`, static build from the pinned tag.
pub const PASST_LINUX_X86_64: NetstackPin = NetstackPin {
    os: "linux",
    arch: "x86_64",
    kind: NetstackKind::Passt,
    version: "2026_10_02.cba3570",
    file_name: "passt",
    git: GitPin {
        repo: "https://passt.top/passt",
        tag: "2026_10_02.cba3570",
        commit: "cba357068dc586e7540971eb40bb56c5e62c4de1",
    },
    release: None,
    // `make static` on the KVM host (Ubuntu 26.04, gcc 15.2, glibc 2.43).
    reference_build_sha256: Some(
        "d1cfac41c63701fc6a0162f6da168320a1ac7c5bfae191040337d3b43dcf217b",
    ),
};

/// The netstack pin for an (os, arch).
#[must_use]
pub fn pin_for(os: &str, arch: &str) -> Option<&'static NetstackPin> {
    [&GVPROXY_MACOS_AARCH64, &PASST_LINUX_X86_64]
        .into_iter()
        .find(|p| p.os == os && p.arch == arch)
}

/// The netstack pin for this host.
pub fn host_pin() -> Result<&'static NetstackPin, ArtifactError> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    pin_for(os, arch).ok_or_else(|| ArtifactError::Unpinned {
        what: format!("userspace netstack for {os}/{arch}"),
        hint: "supported: macOS aarch64 (gvproxy), Linux x86_64 (passt)".into(),
    })
}

/// `<home>/artifacts/libkrun-net/<kind>-<version>/`.
#[must_use]
pub fn default_dir(home: &Path, pin: &NetstackPin) -> PathBuf {
    home.join("artifacts")
        .join(NETSTACK_ARTIFACT_DIR)
        .join(format!("{}-{}", pin.kind, pin.version))
}

/// `manifest.json` of a netstack install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetstackManifest {
    pub schema: u32,
    pub os: String,
    pub arch: String,
    pub kind: NetstackKind,
    pub version: String,
    pub file: String,
    /// `release` (unmodified asset) or `source` (built from `git_commit`).
    pub origin: Origin,
    pub origin_sha256: Option<String>,
    pub git_commit: Option<String>,
    /// Digest of the installed file.
    pub sha256: String,
}

impl NetstackManifest {
    /// Check every field against `pin` (no file I/O).
    pub fn check(&self, pin: &NetstackPin) -> Result<(), String> {
        if self.schema != NETSTACK_MANIFEST_SCHEMA {
            return Err(format!(
                "schema {} (want {NETSTACK_MANIFEST_SCHEMA})",
                self.schema
            ));
        }
        if (self.os.as_str(), self.arch.as_str()) != (pin.os, pin.arch) {
            return Err(format!(
                "installed for {}/{}, pinned for {}/{}",
                self.os, self.arch, pin.os, pin.arch
            ));
        }
        if self.kind != pin.kind || self.version != pin.version || self.file != pin.file_name {
            return Err(format!(
                "{} {} (`{}`) installed, {} {} (`{}`) pinned",
                self.kind, self.version, self.file, pin.kind, pin.version, pin.file_name
            ));
        }
        if !(self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        {
            return Err(format!("sha256 `{}` is malformed", self.sha256));
        }
        match self.origin {
            Origin::Release => {
                let Some((_, sha)) = pin.release else {
                    return Err(format!("no pinned {} release on this platform", pin.kind));
                };
                if self.origin_sha256.as_deref() != Some(sha) || self.sha256 != sha {
                    return Err(format!(
                        "{} release sha {} is not the pinned {sha}",
                        pin.kind, self.sha256
                    ));
                }
            }
            Origin::Source => {
                if self.git_commit.as_deref() != Some(pin.git.commit) {
                    return Err(format!(
                        "{} built from commit {}, pinned {} ({})",
                        pin.kind,
                        self.git_commit.as_deref().unwrap_or("<none>"),
                        pin.git.commit,
                        pin.git.tag
                    ));
                }
            }
            Origin::Homebrew => return Err(format!("{}: no Homebrew pin", pin.kind)),
        }
        Ok(())
    }
}

/// A verified netstack install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNetstack {
    pub kind: NetstackKind,
    pub path: PathBuf,
    pub sha256: String,
    pub manifest: NetstackManifest,
}

/// Resolve the netstack in `dir` against `pin`: `Ok(None)` when nothing is installed there.
pub fn resolve_dir(
    dir: &Path,
    pin: &NetstackPin,
) -> Result<Option<ResolvedNetstack>, ArtifactError> {
    let mpath = dir.join(NETSTACK_MANIFEST);
    let raw = match fs::read(&mpath) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ArtifactError::Io {
                context: format!("read {}", mpath.display()),
                source: e,
            });
        }
    };
    let mismatch = |detail: String| ArtifactError::ManifestMismatch {
        path: mpath.clone(),
        detail,
    };
    let manifest: NetstackManifest =
        serde_json::from_slice(&raw).map_err(|e| mismatch(format!("unparseable: {e}")))?;
    manifest.check(pin).map_err(mismatch)?;
    let path = dir.join(&manifest.file);
    if !path.is_file() {
        return Err(ArtifactError::NotInstalled {
            dir: dir.to_path_buf(),
            detail: format!("missing {}", manifest.file),
        });
    }
    let actual = sha256_file(&path)?;
    if actual != manifest.sha256 {
        return Err(ArtifactError::DigestMismatch {
            path,
            expected: manifest.sha256,
            actual,
        });
    }
    Ok(Some(ResolvedNetstack {
        kind: manifest.kind,
        path,
        sha256: actual,
        manifest,
    }))
}

/// Resolve this host's netstack under `home` (`Ok(None)`: not installed, or no pin here).
pub fn resolve(home: &Path) -> Result<Option<ResolvedNetstack>, ArtifactError> {
    match host_pin() {
        Ok(pin) => resolve_dir(&default_dir(home, pin), pin),
        Err(_) => Ok(None),
    }
}

/// The netstack at [`LibkrunConfig::netstack_dir`](crate::LibkrunConfig::netstack_dir), as
/// [`crate::KrunLibs::netstack`]: `Ok(None)` when unset, unpinned here or not installed.
pub fn resolve_config(
    config: &crate::LibkrunConfig,
) -> Result<Option<crate::NetstackBin>, ArtifactError> {
    let (Some(dir), Ok(pin)) = (&config.netstack_dir, host_pin()) else {
        return Ok(None);
    };
    Ok(resolve_dir(dir, pin)?.map(|r| crate::NetstackBin {
        kind: r.kind,
        binary: crate::helper_proto::PinnedFile {
            path: r.path,
            sha256: r.sha256,
        },
    }))
}

/// Install `src` (a downloaded release asset, or a build of `git_commit`) for `pin` into
/// `default_dir(home, pin)`: verify provenance, copy (0555), write the manifest last, swap
/// the directory into place.
pub fn install_file(
    home: &Path,
    pin: &NetstackPin,
    src: &Path,
    origin: Origin,
    git_commit: Option<&str>,
) -> Result<ResolvedNetstack, ArtifactError> {
    let dest = default_dir(home, pin);
    let origin_sha256 = match origin {
        Origin::Release => {
            let (_, sha) = pin.release.ok_or_else(|| ArtifactError::Unpinned {
                what: format!("{} release on {}/{}", pin.kind, pin.os, pin.arch),
                hint: "build it from source (scripts/fetch-libkrun.sh)".into(),
            })?;
            let actual = sha256_file(src)?;
            if actual != sha {
                return Err(ArtifactError::DigestMismatch {
                    path: src.to_path_buf(),
                    expected: sha.into(),
                    actual,
                });
            }
            Some(sha.to_owned())
        }
        _ => None,
    };
    let manifest = NetstackManifest {
        schema: NETSTACK_MANIFEST_SCHEMA,
        os: pin.os.into(),
        arch: pin.arch.into(),
        kind: pin.kind,
        version: pin.version.into(),
        file: pin.file_name.into(),
        origin,
        origin_sha256,
        git_commit: git_commit.map(str::to_owned),
        sha256: sha256_file(src)?,
    };
    manifest
        .check(pin)
        .map_err(|detail| ArtifactError::ManifestMismatch {
            path: dest.join(NETSTACK_MANIFEST),
            detail,
        })?;

    let parent = dest.parent().expect("versioned dir has a parent");
    let io = |what: String| {
        move |source| ArtifactError::Io {
            context: what,
            source,
        }
    };
    fs::create_dir_all(parent).map_err(io(format!("create {}", parent.display())))?;
    let leaf = dest
        .file_name()
        .expect("leaf")
        .to_string_lossy()
        .into_owned();
    let staging = parent.join(format!(".{leaf}.staging.{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    let res = (|| {
        fs::create_dir(&staging).map_err(io(format!("create {}", staging.display())))?;
        let bin = staging.join(pin.file_name);
        fs::copy(src, &bin).map_err(io(format!("copy {}", src.display())))?;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o555))
            .map_err(io(format!("chmod {}", bin.display())))?;
        let mpath = staging.join(NETSTACK_MANIFEST);
        let mut json = serde_json::to_vec_pretty(&manifest).expect("manifest serializes");
        json.push(b'\n');
        let mut f = fs::File::create(&mpath).map_err(io(format!("create {}", mpath.display())))?;
        f.write_all(&json)
            .and_then(|()| f.sync_all())
            .map_err(io(format!("write {}", mpath.display())))?;
        let old = parent.join(format!(".{leaf}.old.{}", std::process::id()));
        if dest.exists() {
            fs::rename(&dest, &old).map_err(io(format!("move aside {}", dest.display())))?;
        }
        fs::rename(&staging, &dest).map_err(io(format!("install {}", dest.display())))?;
        let _ = fs::remove_dir_all(&old);
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    res?;
    resolve_dir(&dest, pin)?.ok_or_else(|| ArtifactError::NotInstalled {
        dir: dest,
        detail: "manifest vanished after install".into(),
    })
}

/// Download this host's pinned release asset with `curl` (HTTPS only) into a temp dir,
/// verify it and install it. Only for pins with a release (gvproxy on macOS); passt is
/// built from source by `scripts/fetch-libkrun.sh`.
pub fn install_release(home: &Path) -> Result<ResolvedNetstack, ArtifactError> {
    let pin = host_pin()?;
    let Some((url, _)) = pin.release else {
        return Err(ArtifactError::Unpinned {
            what: format!("{} release on {}/{}", pin.kind, pin.os, pin.arch),
            hint: "it is built from the pinned source tag by scripts/fetch-libkrun.sh".into(),
        });
    };
    let tmp = home
        .join("artifacts")
        .join(NETSTACK_ARTIFACT_DIR)
        .join(format!(".download.{}", std::process::id()));
    fs::create_dir_all(&tmp).map_err(|e| ArtifactError::Io {
        context: format!("create {}", tmp.display()),
        source: e,
    })?;
    let file = tmp.join(pin.file_name);
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--retry",
            "3",
            "-o",
        ])
        .arg(&file)
        .arg(url)
        .output();
    let res = match out {
        Ok(o) if o.status.success() => install_file(home, pin, &file, Origin::Release, None),
        Ok(o) => Err(ArtifactError::Command {
            cmd: format!("curl {url}"),
            detail: String::from_utf8_lossy(&o.stderr).trim().into(),
        }),
        Err(e) => Err(ArtifactError::Command {
            cmd: format!("curl {url}"),
            detail: e.to_string(),
        }),
    };
    let _ = fs::remove_dir_all(&tmp);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pin(sha: &'static str) -> NetstackPin {
        NetstackPin {
            release: Some(("https://example.invalid/gvproxy", sha)),
            ..GVPROXY_MACOS_AARCH64
        }
    }

    #[test]
    fn host_pins_match_the_netstack_kind() {
        for p in [&GVPROXY_MACOS_AARCH64, &PASST_LINUX_X86_64] {
            assert_eq!(pin_for(p.os, p.arch), Some(p));
            assert_eq!(p.git.commit.len(), 40);
            if let Some((url, sha)) = p.release {
                assert!(url.starts_with("https://"));
                assert_eq!(sha.len(), 64);
            }
        }
        assert_eq!(GVPROXY_MACOS_AARCH64.kind, NetstackKind::Gvproxy);
        assert_eq!(PASST_LINUX_X86_64.kind, NetstackKind::Passt);
        assert!(
            PASST_LINUX_X86_64.release.is_none(),
            "passt is source-built"
        );
        assert!(pin_for("macos", "x86_64").is_none());
        if let Some(kind) = NetstackKind::for_host()
            && let Ok(pin) = host_pin()
        {
            assert_eq!(pin.kind, kind);
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
        let (g, p) = (&GVPROXY_MACOS_AARCH64, &PASST_LINUX_X86_64);
        let (gurl, gsha) = g.release.unwrap();
        for (key, want) in [
            (
                "NETSTACK_MANIFEST_SCHEMA",
                NETSTACK_MANIFEST_SCHEMA.to_string(),
            ),
            ("GVPROXY_VERSION", g.version.into()),
            ("GVPROXY_COMMIT", g.git.commit.into()),
            ("GVPROXY_URL", gurl.into()),
            ("GVPROXY_SHA256", gsha.into()),
            ("PASST_VERSION", p.version.into()),
            ("PASST_REPO", p.git.repo.into()),
            ("PASST_TAG", p.git.tag.into()),
            ("PASST_COMMIT", p.git.commit.into()),
        ] {
            assert_eq!(get(key), want, "{key}");
        }
        assert!(
            script.contains("BLK=1 NET=1"),
            "libkrun must be built with virtio-net"
        );
        assert!(script.contains("krun_add_net_unixstream"));
    }

    #[test]
    fn release_install_resolves_and_detects_tampering() {
        let home = tempfile::tempdir().unwrap();
        let src = home.path().join("dl");
        fs::write(&src, b"abc").unwrap();
        // sha256("abc")
        let pin = test_pin("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let dir = default_dir(home.path(), &pin);
        assert_eq!(resolve_dir(&dir, &pin).unwrap(), None, "not installed");

        let r = install_file(home.path(), &pin, &src, Origin::Release, None).unwrap();
        assert_eq!(r.kind, NetstackKind::Gvproxy);
        assert_eq!(r.path, dir.join("gvproxy"));
        let mode = fs::metadata(&r.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o555);
        assert_eq!(resolve_dir(&dir, &pin).unwrap(), Some(r.clone()));

        // Swapped binary: digest mismatch, never `None`.
        fs::set_permissions(&r.path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&r.path, b"evil").unwrap();
        assert!(matches!(
            resolve_dir(&dir, &pin),
            Err(ArtifactError::DigestMismatch { .. })
        ));
        // Another pin (e.g. a newer build of sandcrab): the old manifest is refused.
        let other = test_pin("3608bca1e44ea6c4d268eb6db02260269892c0b42b86bbf1e77a6fa16c3c9282");
        assert!(matches!(
            resolve_dir(&dir, &other),
            Err(ArtifactError::ManifestMismatch { .. })
        ));
    }

    #[test]
    fn release_install_refuses_a_wrong_download() {
        let home = tempfile::tempdir().unwrap();
        let src = home.path().join("dl");
        fs::write(&src, b"not the release").unwrap();
        let pin = test_pin("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(matches!(
            install_file(home.path(), &pin, &src, Origin::Release, None),
            Err(ArtifactError::DigestMismatch { .. })
        ));
        assert_eq!(
            resolve_dir(&default_dir(home.path(), &pin), &pin).unwrap(),
            None
        );
    }

    #[test]
    fn source_install_needs_the_pinned_commit() {
        let home = tempfile::tempdir().unwrap();
        let src = home.path().join("passt");
        fs::write(&src, b"\x7fELF...").unwrap();
        let pin = PASST_LINUX_X86_64;
        let err = install_file(
            home.path(),
            &pin,
            &src,
            Origin::Source,
            Some(&"0".repeat(40)),
        );
        assert!(matches!(err, Err(ArtifactError::ManifestMismatch { .. })));
        let r = install_file(
            home.path(),
            &pin,
            &src,
            Origin::Source,
            Some(pin.git.commit),
        )
        .unwrap();
        assert_eq!(r.kind, NetstackKind::Passt);
        assert_eq!(r.manifest.git_commit.as_deref(), Some(pin.git.commit));
        // A release origin is refused for a source-only pin.
        assert!(install_file(home.path(), &pin, &src, Origin::Release, None).is_err());
    }
}
