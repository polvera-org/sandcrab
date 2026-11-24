//! Rootless `mke2fs` invocation strategies.
//!
//! The image must keep the tar's uid/gid/modes while we run as an unprivileged user:
//! - [`MkfsStrategy::NativeTar`]: host `mke2fs -d <tarball>` (e2fsprogs ≥ 1.47.1 built with libarchive).
//!   Fastest; ownership comes straight from tar headers.
//! - [`MkfsStrategy::Container`]: the same `mke2fs -d <tarball>` inside a small helper image
//!   (`debian:trixie-slim` + e2fsprogs 1.47.2 + libarchive), run as the calling uid with `--network none`.
//!   The design doc's documented fallback.
//! - [`MkfsStrategy::Fakeroot`]: extract under `fakeroot` and `mke2fs -d <dir>` in the same session so the
//!   faked ownership is what mke2fs `lstat`s. Slowest (full extraction to disk); last resort.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, IoCtx, Result};

/// Helper image tag for [`MkfsStrategy::Container`].
pub const MKFS_IMAGE: &str = "sandcrab/mkfs:0.1.0";

/// Dockerfile for [`MKFS_IMAGE`]; base pinned like `templates/base`.
pub const MKFS_DOCKERFILE: &str = "\
FROM debian:trixie-20261104-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
RUN apt-get update \\
 && apt-get install -y --no-install-recommends e2fsprogs libarchive13t64 \\
 && rm -rf /var/lib/apt/lists/*
";

/// Fixed fs timestamp (`E2FSPROGS_FAKE_TIME`) for best-effort reproducible images.
pub const FAKE_TIME: &str = "1700000000";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MkfsStrategy {
    NativeTar,
    Container,
    Fakeroot,
}

impl std::str::FromStr for MkfsStrategy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "native" | "native_tar" => Ok(Self::NativeTar),
            "container" => Ok(Self::Container),
            "fakeroot" => Ok(Self::Fakeroot),
            o => Err(format!("unknown mkfs strategy {o:?}")),
        }
    }
}

/// Parameters for one `mke2fs` run.
#[derive(Debug, Clone)]
pub struct MkfsParams {
    pub label: String,
    pub uuid: String,
    pub size_bytes: u64,
    pub inodes: u64,
    /// Create a journal (writable workspace drives). Read-only images have none.
    pub journal: bool,
    /// Owner of the image's root directory (`-E root_owner=uid:gid`); `None` = root.
    pub root_owner: Option<(u32, u32)>,
}

impl MkfsParams {
    fn args(&self, src: &str, img: &str) -> Vec<String> {
        let mut extended = format!("hash_seed={}", self.uuid);
        if let Some((uid, gid)) = self.root_owner {
            extended.push_str(&format!(",root_owner={uid}:{gid}"));
        }
        let mut a: Vec<String> = vec![
            "-q".into(),
            "-F".into(),
            "-t".into(),
            "ext4".into(),
            "-b".into(),
            "4096".into(),
            "-m".into(),
            "0".into(),
        ];
        if !self.journal {
            a.extend(["-O".into(), "^has_journal".into()]);
        }
        a.extend([
            "-L".into(),
            self.label.clone(),
            "-U".into(),
            self.uuid.clone(),
            "-E".into(),
            extended,
            "-N".into(),
            self.inodes.to_string(),
            "-d".into(),
            src.into(),
            img.into(),
            format!("{}k", self.size_bytes / 1024),
        ]);
        a
    }
}

/// Run `cmd` with stdin closed and return its stdout; a non-zero exit is [`Error::Command`].
pub fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::Command {
            cmd: format!("{cmd:?}"),
            status: "spawn failed".into(),
            stderr: e.to_string(),
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(Error::Command {
            cmd: format!("{cmd:?}"),
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// Parse `mke2fs -V` output (`mke2fs 1.47.2 (1-Jan-2025)`) into (major, minor, patch).
pub fn parse_e2fs_version(s: &str) -> Option<(u32, u32, u32)> {
    let v = s
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()) && w.contains('.'))?;
    let mut it = v.split('.').map(|p| {
        p.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .ok()
    });
    Some((it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
}

fn host_e2fs_version() -> Option<(u32, u32, u32)> {
    let out = Command::new("mke2fs").arg("-V").output().ok()?;
    // mke2fs prints the version on stderr.
    parse_e2fs_version(&String::from_utf8_lossy(&out.stderr))
        .or_else(|| parse_e2fs_version(&String::from_utf8_lossy(&out.stdout)))
}

/// Does the host mke2fs accept tarballs for `-d`? (version ≥ 1.47.1 *and* libarchive loadable).
pub fn probe_native(scratch_dir: &Path) -> bool {
    if host_e2fs_version().is_none_or(|v| v < (1, 47, 1)) {
        return false;
    }
    let tar_path = scratch_dir.join(".probe.tar");
    let img = scratch_dir.join(".probe.ext4");
    let ok = (|| -> Result<bool> {
        let mut b = tar::Builder::new(fs::File::create(&tar_path).at(&tar_path)?);
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o644);
        h.set_uid(4242);
        b.append_data(&mut h, "probe", &b"x"[..])?;
        b.into_inner()?;
        Ok(Command::new("mke2fs")
            .args(["-q", "-F", "-t", "ext4", "-d"])
            .arg(&tar_path)
            .arg(&img)
            .arg("4M")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false))
    })()
    .unwrap_or(false);
    let _ = fs::remove_file(&tar_path);
    let _ = fs::remove_file(&img);
    ok
}

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Pick the best available strategy: native → container → fakeroot.
pub fn detect(scratch_dir: &Path) -> Result<MkfsStrategy> {
    if probe_native(scratch_dir) {
        return Ok(MkfsStrategy::NativeTar);
    }
    if have("docker") {
        return Ok(MkfsStrategy::Container);
    }
    if have("fakeroot") {
        return Ok(MkfsStrategy::Fakeroot);
    }
    Err(Error::NoMkfs(
        "need e2fsprogs >= 1.47.1 with libarchive, docker, or fakeroot".into(),
    ))
}

/// Build [`MKFS_IMAGE`] if it is not present locally.
pub fn ensure_mkfs_image() -> Result<()> {
    let present = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", MKFS_IMAGE])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if present {
        return Ok(());
    }
    let dir = tempdir_in(&std::env::temp_dir(), "sandcrab-mkfs-img")?;
    let df = dir.join("Dockerfile");
    fs::write(&df, MKFS_DOCKERFILE).at(&df)?;
    let res = run(Command::new("docker")
        .args(["build", "--pull=false", "-q", "-t", MKFS_IMAGE])
        .arg(&dir));
    let _ = fs::remove_dir_all(&dir);
    res.map(|_| ())
}

pub fn tempdir_in(parent: &Path, prefix: &str) -> Result<PathBuf> {
    fs::create_dir_all(parent).at(parent)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = parent.join(format!(".{prefix}-{}-{nanos}", std::process::id()));
    fs::create_dir(&p).at(&p)?;
    Ok(p)
}

/// Run mke2fs with `tar_path` as the source, producing `img` (both inside `work_dir`).
pub fn mkfs_from_tar(
    strategy: MkfsStrategy,
    work_dir: &Path,
    tar_path: &Path,
    img: &Path,
    params: &MkfsParams,
) -> Result<()> {
    match strategy {
        MkfsStrategy::NativeTar => {
            run(Command::new("mke2fs")
                .env("E2FSPROGS_FAKE_TIME", FAKE_TIME)
                .args(params.args(&path_str(tar_path), &path_str(img))))?;
        }
        MkfsStrategy::Container => {
            ensure_mkfs_image()?;
            let meta = fs::metadata(work_dir).at(work_dir)?;
            let rel = |p: &Path| -> Result<String> {
                let name = p
                    .strip_prefix(work_dir)
                    .map_err(|_| Error::Unsupported(format!("{} not in work dir", p.display())))?;
                Ok(format!("/w/{}", name.display()))
            };
            run(Command::new("docker")
                .args(["run", "--rm", "--network", "none", "--user"])
                .arg(format!("{}:{}", meta.uid(), meta.gid()))
                .arg("-v")
                .arg(format!("{}:/w", path_str(work_dir)))
                .args(["-e", &format!("E2FSPROGS_FAKE_TIME={FAKE_TIME}")])
                .args([MKFS_IMAGE, "mke2fs"])
                .args(params.args(&rel(tar_path)?, &rel(img)?)))?;
        }
        MkfsStrategy::Fakeroot => {
            let stage = work_dir.join("stage");
            fs::create_dir_all(&stage).at(&stage)?;
            let mut script = String::from(
                "set -e; tar --numeric-owner --xattrs -xpf \"$1\" -C \"$2\"; shift 2;",
            );
            script.push_str(" E2FSPROGS_FAKE_TIME=");
            script.push_str(FAKE_TIME);
            script.push_str(" exec mke2fs \"$@\"");
            let res = run(Command::new("fakeroot")
                .args(["--", "sh", "-c", &script, "sh"])
                .arg(tar_path)
                .arg(&stage)
                .args(params.args(&path_str(&stage), &path_str(img))));
            let _ = fs::remove_dir_all(&stage);
            res?;
        }
    }
    Ok(())
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(
            parse_e2fs_version("mke2fs 1.47.2 (1-Jan-2025)\n\tUsing EXT2FS Library version 1.47.2"),
            Some((1, 47, 2))
        );
        assert_eq!(
            parse_e2fs_version("mke2fs 1.46.5 (30-Dec-2021)"),
            Some((1, 46, 5))
        );
        assert_eq!(parse_e2fs_version("garbage"), None);
    }

    #[test]
    fn strategy_from_str() {
        assert_eq!(
            "native".parse::<MkfsStrategy>(),
            Ok(MkfsStrategy::NativeTar)
        );
        assert!("nope".parse::<MkfsStrategy>().is_err());
    }
}
