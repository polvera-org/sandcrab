//! Pinned guest kernel + Firecracker release artifacts (design doc §1).
//!
//! Downloaded once into `<cache>/kernel/<version>/vmlinux` and
//! `<cache>/firecracker/<version>/{firecracker,jailer}`, verified by SHA-256 on download **and** on
//! every cache hit. A mismatch is a hard error; nothing is ever used unverified.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, IoCtx, Result};
use crate::hash::{sha256_file, verify_sha256};

/// A single pinned download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelPin {
    pub version: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

/// The guest kernel: Firecracker CI build of 6.18 (`microvm-kernel-ci-x86_64-6.18.config`), from the
/// newest dated CI prefix at pin time. (The design doc's `firecracker-ci/v1.18/` prefix is empty; CI
/// artifacts moved to dated `firecracker-ci/<YYYYMMDD>-<sha>-0/` prefixes.)
pub const KERNEL_X86_64: KernelPin = KernelPin {
    version: "6.18.62",
    url: "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20261118-6d7cfa9c6b53-0/x86_64/vmlinux-6.18.62",
    sha256: "6b2871bb1e035259a58d1b920fd8951e580125fda17825347bd475911fd2d069",
};

/// A pinned Firecracker release tarball plus the digests of the members we extract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirecrackerPin {
    pub version: &'static str,
    pub url: &'static str,
    /// Tarball digest (matches upstream `firecracker-v1.18.0-x86_64.tgz.sha256.txt`).
    pub tgz_sha256: &'static str,
    pub firecracker_member: &'static str,
    pub firecracker_sha256: &'static str,
    pub jailer_member: &'static str,
    pub jailer_sha256: &'static str,
}

pub const FIRECRACKER_X86_64: FirecrackerPin = FirecrackerPin {
    version: "1.18.0",
    url: "https://github.com/firecracker-microvm/firecracker/releases/download/v1.18.0/firecracker-v1.18.0-x86_64.tgz",
    tgz_sha256: "1ac8eec866dd5eea14efecf6323e22b23eea24054545338a5b34bc5bca6623cd",
    firecracker_member: "release-v1.18.0-x86_64/firecracker-v1.18.0-x86_64",
    firecracker_sha256: "b33f46cd6d88834cf6a4f8d184f08f219809c636236bffd286d233a60602a2ba",
    jailer_member: "release-v1.18.0-x86_64/jailer-v1.18.0-x86_64",
    jailer_sha256: "ddadbaa15b5c942271a83b7040ca7259c8ccf92a9e08d44cce0b86fa407ba460",
};

fn require_x86_64() -> Result<()> {
    if std::env::consts::ARCH == "x86_64" {
        Ok(())
    } else {
        Err(Error::Unsupported(format!(
            "no pinned Firecracker artifacts for {} (x86_64 only in V1)",
            std::env::consts::ARCH
        )))
    }
}

/// Paths of the verified Firecracker binaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirecrackerArtifacts {
    pub firecracker: PathBuf,
    pub jailer: Option<PathBuf>,
}

/// Download the pinned kernel (if needed) and return the verified `vmlinux` path.
pub fn fetch_kernel(cache_root: &Path) -> Result<PathBuf> {
    require_x86_64()?;
    fetch_kernel_pin(cache_root, &KERNEL_X86_64)
}

pub fn fetch_kernel_pin(cache_root: &Path, pin: &KernelPin) -> Result<PathBuf> {
    let dir = cache_root.join("kernel").join(pin.version);
    let dest = dir.join("vmlinux");
    if dest.is_file() {
        verify_sha256(&dest, pin.sha256)?;
        return Ok(dest);
    }
    fs::create_dir_all(&dir).at(&dir)?;
    let tmp = dir.join(format!(".vmlinux.{}.part", std::process::id()));
    let res = download(pin.url, &tmp).and_then(|()| verify_sha256(&tmp, pin.sha256));
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o444)).at(&tmp)?;
    fs::rename(&tmp, &dest).at(&dest)?;
    Ok(dest)
}

/// Download the pinned Firecracker release (if needed) and return the verified binaries.
pub fn fetch_firecracker(cache_root: &Path, with_jailer: bool) -> Result<FirecrackerArtifacts> {
    require_x86_64()?;
    fetch_firecracker_pin(cache_root, &FIRECRACKER_X86_64, with_jailer)
}

pub fn fetch_firecracker_pin(
    cache_root: &Path,
    pin: &FirecrackerPin,
    with_jailer: bool,
) -> Result<FirecrackerArtifacts> {
    let dir = cache_root.join("firecracker").join(pin.version);
    let fc = dir.join("firecracker");
    let jailer = dir.join("jailer");
    let have_fc = fc.is_file();
    let have_jailer = !with_jailer || jailer.is_file();
    if !(have_fc && have_jailer) {
        fs::create_dir_all(&dir).at(&dir)?;
        let tgz = dir.join(format!(".release.{}.tgz.part", std::process::id()));
        let res = download(pin.url, &tgz)
            .and_then(|()| verify_sha256(&tgz, pin.tgz_sha256))
            .and_then(|()| {
                let mut wanted = vec![(pin.firecracker_member, pin.firecracker_sha256, &fc)];
                if with_jailer {
                    wanted.push((pin.jailer_member, pin.jailer_sha256, &jailer));
                }
                extract_members(&tgz, &wanted)
            });
        let _ = fs::remove_file(&tgz);
        res?;
    }
    verify_sha256(&fc, pin.firecracker_sha256)?;
    if with_jailer {
        verify_sha256(&jailer, pin.jailer_sha256)?;
    }
    Ok(FirecrackerArtifacts {
        firecracker: fc,
        jailer: with_jailer.then_some(jailer),
    })
}

/// Extract selected members of a `.tgz` to destination paths, verifying each digest before the
/// atomic rename into place.
fn extract_members(tgz: &Path, wanted: &[(&str, &str, &PathBuf)]) -> Result<()> {
    let f = File::open(tgz).at(tgz)?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    let mut found = vec![false; wanted.len()];
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let path = path.trim_start_matches("./");
        let Some(i) = wanted.iter().position(|(m, _, _)| *m == path) else {
            continue;
        };
        let (_, sha, dest) = wanted[i];
        let tmp = dest.with_extension("part");
        {
            let mut out = File::create(&tmp).at(&tmp)?;
            io::copy(&mut (&mut entry).take(512 << 20), &mut out).at(&tmp)?;
            out.flush().at(&tmp)?;
        }
        if let Err(e) = verify_sha256(&tmp, sha) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).at(&tmp)?;
        fs::rename(&tmp, dest).at(dest)?;
        found[i] = true;
    }
    if let Some(i) = found.iter().position(|f| !f) {
        return Err(Error::Unsupported(format!(
            "{} not found in {}",
            wanted[i].0,
            tgz.display()
        )));
    }
    Ok(())
}

/// HTTPS download via `curl` (ubiquitous; keeps an HTTP/TLS stack out of this crate).
fn download(url: &str, dest: &Path) -> Result<()> {
    crate::rootfs::mkfs::run(
        Command::new("curl")
            .args([
                "-fsSL",
                "--proto",
                "=https",
                "--tlsv1.2",
                "--retry",
                "3",
                "-o",
            ])
            .arg(dest)
            .arg(url),
    )?;
    Ok(())
}

/// Hex digest helper re-exported for `doctor`-style checks.
pub fn digest(path: &Path) -> Result<String> {
    sha256_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_are_well_formed() {
        for d in [
            KERNEL_X86_64.sha256,
            FIRECRACKER_X86_64.tgz_sha256,
            FIRECRACKER_X86_64.firecracker_sha256,
            FIRECRACKER_X86_64.jailer_sha256,
        ] {
            assert_eq!(d.len(), 64);
            assert!(
                d.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            );
        }
        assert!(KERNEL_X86_64.url.ends_with(KERNEL_X86_64.version));
        assert!(KERNEL_X86_64.version.starts_with("6.18."));
        assert!(FIRECRACKER_X86_64.url.contains(FIRECRACKER_X86_64.version));
        assert!(FIRECRACKER_X86_64.url.starts_with("https://"));
    }

    fn tgz_with(member: &str, body: &[u8]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        {
            let mut b = tar::Builder::new(&mut gz);
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o755);
            b.append_data(&mut h, member, body).unwrap();
            b.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    #[test]
    fn extract_verifies_member_digest() {
        let dir = tempfile::tempdir().unwrap();
        let tgz = dir.path().join("r.tgz");
        fs::write(&tgz, tgz_with("rel/fc", b"abc")).unwrap();
        let dest = dir.path().join("firecracker");
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        extract_members(&tgz, &[("rel/fc", good, &dest)]).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"abc");
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o755
        );

        let dest2 = dir.path().join("fc2");
        assert!(matches!(
            extract_members(&tgz, &[("rel/fc", &"0".repeat(64), &dest2)]),
            Err(Error::DigestMismatch { .. })
        ));
        assert!(!dest2.exists());
        assert!(extract_members(&tgz, &[("rel/missing", good, &dest2)]).is_err());
    }

    #[test]
    fn kernel_cache_hit_is_verified() {
        let dir = tempfile::tempdir().unwrap();
        let pin = KernelPin {
            version: "0.0.0",
            url: "https://invalid.example/never-fetched",
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        let p = dir.path().join("kernel/0.0.0/vmlinux");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"abc").unwrap();
        assert_eq!(fetch_kernel_pin(dir.path(), &pin).unwrap(), p);
        fs::write(&p, b"tampered").unwrap();
        assert!(matches!(
            fetch_kernel_pin(dir.path(), &pin),
            Err(Error::DigestMismatch { .. })
        ));
    }

    /// Network test: downloads ~35 MB from S3/GitHub.
    #[test]
    #[ignore = "network: downloads the pinned kernel + firecracker release"]
    fn fetch_real_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let k = fetch_kernel(dir.path()).unwrap();
        assert!(k.ends_with("kernel/6.18.62/vmlinux"));
        let fc = fetch_firecracker(dir.path(), true).unwrap();
        assert!(fc.jailer.is_some());
        // second call is a verified cache hit
        assert_eq!(fetch_firecracker(dir.path(), true).unwrap(), fc);
    }
}
