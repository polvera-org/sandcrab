//! Stat stamps: verify a pinned file with one `fstat` instead of a full SHA-256, and use it
//! through the very fd that was checked (libkrun design §12.4, review decisions (a) and (b)).
//!
//! A [`FileStamp`] is `(st_dev, st_ino, st_size, st_mtime, st_ctime)` in nanoseconds. It is
//! recorded next to a file's SHA-256 once the content has been hashed **through the same fd**
//! and the file did not change while it was read. Later, [`open_verified`] opens the file once,
//! `fstat`s the fd and compares: a match means "the inode that was hashed, unmodified since",
//! so the recorded digest still holds and nothing is read. A mismatch re-hashes the content
//! through that fd: a good digest yields a fresh stamp (the caller re-records it), a bad one is
//! a [`Error::DigestMismatch`]. Either way the caller then uses the returned fd (or a
//! `/proc/self/fd/<n>` path to it), never the path again, so the check and the use refer to
//! the same inode.
//!
//! # Why `ctime` cannot be forged by the same unprivileged user
//!
//! Every write, truncate, `chmod`/`chown`, link-count change and `utimensat` sets `st_ctime`
//! to the kernel's current time, and no system call lets a process set `st_ctime` to a chosen
//! value (`utimensat` sets atime/mtime only, and bumps ctime to *now*). Replacing the file
//! (rename-over, unlink + create) yields another inode, or a reused inode number with a new
//! ctime. Equal ctime therefore means "not modified since the stamp", unless the attacker can
//! set the system clock or write the block device: both need root, which owns the hypervisor
//! anyway.
//!
//! # Limits (documented in `docs/design/libkrun-backend.md` §12.4)
//!
//! - **Root**: can reset the clock, write the raw device, or use a debugfs to rewrite inodes.
//! - **Filesystems with unreliable ctime**: network filesystems (NFS/SMB: the server's clock
//!   and caching), FUSE (the daemon reports attributes; its `st_dev` is kernel-assigned per
//!   mount, so an over-mount still changes `dev`), and filesystems with coarse or no ctime
//!   (FAT/exFAT). Keep `SANDCRAB_HOME` on a local ext4/xfs/btrfs/APFS volume.
//! - **Coarse timestamps**: a modification within the same clock tick as the stamp keeps
//!   ctime equal. [`hash_and_stamp`] therefore only records a stamp whose ctime is at least
//!   [`RACY_MARGIN`] older than the start of the hash (git's "racily clean" rule), sleeping
//!   up to that margin when a file was just written.
//! - **Bit rot / media corruption** below the filesystem leaves metadata unchanged: a stamp
//!   does not detect it. `sandcrab rootfs verify` / `doctor` re-hash on demand.

use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, IoCtx, Result};
use crate::hash::to_hex;

/// A stamp is only recorded when the file's ctime is at least this much older than the start of
/// the hash that proved its content (coarse kernel clocks tick every 1 to 10 ms).
pub const RACY_MARGIN: Duration = Duration::from_millis(100);

/// `(dev, ino, size, mtime, ctime)` of a regular file; times in nanoseconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileStamp {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl FileStamp {
    /// The stamp of already-fetched metadata.
    #[must_use]
    pub fn of(m: &Metadata) -> Self {
        let ns = |s: i64, n: i64| s.saturating_mul(1_000_000_000).saturating_add(n);
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime_ns: ns(m.mtime(), m.mtime_nsec()),
            ctime_ns: ns(m.ctime(), m.ctime_nsec()),
        }
    }

    /// `fstat` of an open file.
    pub fn of_file(f: &File) -> io::Result<Self> {
        Ok(Self::of(&f.metadata()?))
    }

    /// The ctime as a [`SystemTime`] (`None` before the epoch).
    fn ctime(&self) -> Option<SystemTime> {
        u64::try_from(self.ctime_ns)
            .ok()
            .map(|ns| UNIX_EPOCH + Duration::from_nanos(ns))
    }
}

/// How [`open_verified`] established the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// The recorded stamp matched: nothing was read.
    Stamp,
    /// The content was hashed through the fd and matched the expected digest.
    Hashed,
}

/// A file opened once and verified through that fd.
#[derive(Debug)]
pub struct VerifiedFile {
    /// Read-only, `O_CLOEXEC`. Use it (or `/proc/self/fd/<n>`), not the path.
    pub file: File,
    /// The stamp to record: the matched one, or a fresh one after a successful re-hash
    /// (`None` when the file was too new or changed while it was hashed; re-hash next time).
    pub stamp: Option<FileStamp>,
    pub how: Verification,
}

impl VerifiedFile {
    /// `true` when the caller should persist [`Self::stamp`] (it differs from `recorded`).
    #[must_use]
    pub fn restamped(&self, recorded: Option<&FileStamp>) -> bool {
        self.stamp.is_some() && self.stamp.as_ref() != recorded
    }
}

/// Open `path` (read-only, close-on-exec), require a regular file and verify it against
/// `expected_sha256` (lowercase hex, optional `sha256:` prefix): a matching `recorded` stamp
/// skips the hash; otherwise the content is hashed through the fd. See the module doc.
pub fn open_verified(
    path: &Path,
    expected_sha256: &str,
    recorded: Option<&FileStamp>,
) -> Result<VerifiedFile> {
    let file = File::open(path).at(path)?;
    verify_open(file, path, expected_sha256, recorded)
}

/// [`open_verified`] for an already-open file (`path` is only used in errors).
pub fn verify_open(
    file: File,
    path: &Path,
    expected_sha256: &str,
    recorded: Option<&FileStamp>,
) -> Result<VerifiedFile> {
    let meta = file.metadata().at(path)?;
    if !meta.is_file() {
        return Err(Error::Unsupported(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let now = FileStamp::of(&meta);
    if recorded == Some(&now) {
        return Ok(VerifiedFile {
            file,
            stamp: Some(now),
            how: Verification::Stamp,
        });
    }
    let (actual, stamp) = hash_and_stamp(&file).at(path)?;
    let expected = expected_sha256
        .strip_prefix("sha256:")
        .unwrap_or(expected_sha256);
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(Error::DigestMismatch {
            what: path.display().to_string(),
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(VerifiedFile {
        file,
        stamp,
        how: Verification::Hashed,
    })
}

/// SHA-256 of `file`'s content (positional reads, the fd offset is untouched) and the stamp
/// that content goes with: `None` when the file changed while it was read, or its ctime is
/// not [`RACY_MARGIN`] older than the hash (after waiting up to that margin).
pub fn hash_and_stamp(file: &File) -> io::Result<(String, Option<FileStamp>)> {
    let before = FileStamp::of_file(file)?;
    let settled = settle(&before);
    let sha = sha256_fd(file)?;
    let after = FileStamp::of_file(file)?;
    Ok((sha, (settled && before == after).then_some(before)))
}

/// Wait (at most [`RACY_MARGIN`]) until `stamp`'s ctime is that much in the past. `false` when
/// it is further in the future than the margin (clock skew): do not record such a stamp.
fn settle(stamp: &FileStamp) -> bool {
    let Some(ctime) = stamp.ctime() else {
        return true;
    };
    let ready = ctime + RACY_MARGIN;
    match ready.duration_since(SystemTime::now()) {
        Err(_) => true,
        Ok(wait) if wait <= RACY_MARGIN => {
            std::thread::sleep(wait);
            true
        }
        Ok(_) => false,
    }
}

/// Lowercase hex SHA-256 of an open file, read with `pread` from offset 0.
pub fn sha256_fd(file: &File) -> io::Result<String> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    loop {
        let n = match file.read_at(&mut buf, off) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        off += n as u64;
    }
    Ok(to_hex(&h.finalize()))
}

/// Hash `path` and return its digest with a stamp (install / build time). Errors if the file
/// changed while it was hashed, so a recorded (sha, stamp) pair always belongs together.
pub fn record(path: &Path) -> Result<(String, FileStamp)> {
    let file = File::open(path).at(path)?;
    let (sha, stamp) = hash_and_stamp(&file).at(path)?;
    let stamp = stamp.ok_or_else(|| {
        Error::Unsupported(format!(
            "{} changed while it was hashed (or its ctime is in the future)",
            path.display()
        ))
    })?;
    Ok((sha, stamp))
}

/// Path through which another open of the file reaches exactly this fd's inode:
/// `/proc/self/fd/<n>` (Linux) or `/dev/fd/<n>` (macOS, fdesc). Valid in this process only
/// (and in a child that inherits the fd).
#[must_use]
pub fn fd_path(fd: std::os::fd::RawFd) -> std::path::PathBuf {
    if cfg!(target_os = "linux") {
        format!("/proc/self/fd/{fd}").into()
    } else {
        format!("/dev/fd/{fd}").into()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::fd::AsRawFd;

    use super::*;
    use crate::hash::sha256_bytes;

    fn write(p: &Path, data: &[u8]) {
        let mut f = File::create(p).unwrap();
        f.write_all(data).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn record_then_stamp_hit_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("img");
        write(&p, b"rootfs");
        let (sha, stamp) = record(&p).unwrap();
        assert_eq!(sha, sha256_bytes(b"rootfs"));
        let v = open_verified(&p, &sha, Some(&stamp)).unwrap();
        assert_eq!(v.how, Verification::Stamp);
        assert!(!v.restamped(Some(&stamp)));
        // A wrong expected digest is not even looked at on a stamp hit: the stamp *is* the
        // proof that the content hashed to the recorded digest.
        assert_eq!(
            open_verified(&p, "00", Some(&stamp)).unwrap().how,
            Verification::Stamp
        );
    }

    #[test]
    fn in_place_modification_is_rehashed_and_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("img");
        write(&p, b"rootfs");
        let (sha, stamp) = record(&p).unwrap();
        // Same size, same mtime restored: only ctime (and the content) tell.
        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        write(&p, b"evilfs");
        File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let now = FileStamp::of(&std::fs::metadata(&p).unwrap());
        assert_eq!((now.size, now.mtime_ns), (stamp.size, stamp.mtime_ns));
        assert_ne!(now.ctime_ns, stamp.ctime_ns);
        match open_verified(&p, &sha, Some(&stamp)) {
            Err(Error::DigestMismatch { .. }) => {}
            other => panic!("expected DigestMismatch, got {other:?}"),
        }
    }

    #[test]
    fn replacement_under_the_same_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("img");
        write(&p, b"rootfs");
        let (sha, stamp) = record(&p).unwrap();
        let evil = dir.path().join("evil");
        write(&evil, b"evilfs");
        std::fs::rename(&evil, &p).unwrap();
        assert!(matches!(
            open_verified(&p, &sha, Some(&stamp)),
            Err(Error::DigestMismatch { .. })
        ));
    }

    #[test]
    fn benign_metadata_change_rehashes_and_restamps() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("img");
        write(&p, b"rootfs");
        let (sha, stamp) = record(&p).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444)).unwrap();
        let v = open_verified(&p, &sha, Some(&stamp)).unwrap();
        assert_eq!(v.how, Verification::Hashed);
        assert!(v.restamped(Some(&stamp)), "fresh stamp to record");
        let fresh = v.stamp.unwrap();
        assert_eq!(
            open_verified(&p, &sha, Some(&fresh)).unwrap().how,
            Verification::Stamp
        );
    }

    #[test]
    fn verified_fd_survives_a_path_swap() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("img");
        write(&p, b"rootfs");
        let (sha, stamp) = record(&p).unwrap();
        let v = open_verified(&p, &sha, Some(&stamp)).unwrap();
        std::fs::rename(&p, dir.path().join("moved")).unwrap();
        write(&p, b"evilfs");
        let reopened = std::fs::read(fd_path(v.file.as_raw_fd())).unwrap();
        assert_eq!(reopened, b"rootfs");
    }

    #[test]
    fn fresh_files_are_settled_before_stamping() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("img");
        write(&p, b"x");
        let t0 = std::time::Instant::now();
        let (_, stamp) = record(&p).unwrap();
        // The file was written just now: record waited until its ctime was RACY_MARGIN old.
        assert!(t0.elapsed() + Duration::from_millis(15) >= RACY_MARGIN);
        assert!(stamp.ctime().unwrap() + RACY_MARGIN <= SystemTime::now());
    }

    #[test]
    fn non_regular_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            open_verified(dir.path(), "00", None),
            Err(Error::Unsupported(_))
        ));
    }
}
