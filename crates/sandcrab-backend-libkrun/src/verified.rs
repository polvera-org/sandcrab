//! Pinned files used through verified fds (design §12.4): the helper side of the stat-stamp
//! scheme (`sandcrab_rootfs::stamp`), and the `sandcrab-krun` helper's own install record.
//!
//! Every pinned input (libkrun, libkrunfw, the root disk, the netstack, the helper itself) is
//! opened **once**, checked through that fd (a matching stat stamp, else a full SHA-256 of the
//! fd), and then used through the same fd: `dlopen("/proc/self/fd/<n>")` (macOS
//! `/dev/fd/<n>`), `krun_add_disk` of that path, and on Linux `execve("/proc/self/fd/<n>")`.
//! So nothing is re-resolved by path between the check and the use, except where the platform
//! offers no fd-based use (macOS `exec`, see the design doc's residual windows).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use sandcrab_rootfs::stamp::{self, FileStamp, VerifiedFile};

use crate::helper_proto::{DiskSpec, PinnedFile};

/// Why a pinned file was refused.
#[derive(Debug, thiserror::Error)]
pub enum PinError {
    /// The content (or, without a digest, the stamp) is not the pinned one.
    #[error("{0}")]
    Mismatch(String),
    /// Could not be opened or read.
    #[error("{0}")]
    Io(String),
}

impl From<sandcrab_rootfs::Error> for PinError {
    fn from(e: sandcrab_rootfs::Error) -> Self {
        match e {
            sandcrab_rootfs::Error::DigestMismatch { .. } => Self::Mismatch(e.to_string()),
            other => Self::Io(other.to_string()),
        }
    }
}

/// Open `pin` once and verify it through that fd (stamp hit, else SHA-256).
pub fn open_pinned(pin: &PinnedFile) -> Result<VerifiedFile, PinError> {
    Ok(stamp::open_verified(
        &pin.path,
        &pin.sha256,
        pin.stamp.as_ref(),
    )?)
}

/// Open a disk the helper hands to libkrun by fd: `None` when it carries neither a stamp nor a
/// digest (scratch, workspace volumes: those go by path). With a stamp, the fd must match it,
/// else the content must hash to [`DiskSpec::sha256`] (no digest: refused). Read-only disks are
/// opened read-only, others read-write (macOS `/dev/fd/<n>` cannot widen the access mode).
pub fn open_disk(d: &DiskSpec) -> Result<Option<File>, PinError> {
    if d.stamp.is_none() && d.sha256.is_none() {
        return Ok(None);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(!d.read_only)
        .open(&d.path)
        .map_err(|e| PinError::Io(format!("opening disk `{}`: {e}", d.path.display())))?;
    match (&d.sha256, &d.stamp) {
        (Some(sha), stamp) => Ok(Some(
            stamp::verify_open(file, &d.path, sha, stamp.as_ref())?.file,
        )),
        (None, Some(want)) => {
            let now = FileStamp::of_file(&file)
                .map_err(|e| PinError::Io(format!("stat `{}`: {e}", d.path.display())))?;
            if &now == want {
                Ok(Some(file))
            } else {
                Err(PinError::Mismatch(format!(
                    "disk `{}` changed after the backend checked it ({want:?} → {now:?})",
                    d.path.display()
                )))
            }
        }
        (None, None) => unreachable!("handled above"),
    }
}

/// Path that reaches `file`'s inode through its fd (`/proc/self/fd/<n>` | `/dev/fd/<n>`).
#[must_use]
pub fn fd_path(file: &File) -> PathBuf {
    stamp::fd_path(file.as_raw_fd())
}

// ---------------------------------------------------------------------------------------------
// The helper's install record

/// Schema of [`HelperRecord`].
pub const HELPER_RECORD_SCHEMA: u32 = 1;

/// `<helper>.pin.json`, written by `sandcrab libkrun install` once the helper is in place
/// (after signing and the rename, which both change its ctime): its SHA-256 and stat stamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperRecord {
    pub schema: u32,
    pub sha256: String,
    pub stamp: FileStamp,
}

/// Where the record of `helper` lives.
#[must_use]
pub fn helper_record_path(helper: &Path) -> PathBuf {
    let mut name = helper
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".pin.json");
    helper.with_file_name(name)
}

impl HelperRecord {
    /// Hash + stamp `helper` as it is now and write its record (atomically).
    pub fn record(helper: &Path) -> Result<Self, PinError> {
        let (sha256, stamp) = stamp::record(helper)?;
        let rec = Self {
            schema: HELPER_RECORD_SCHEMA,
            sha256,
            stamp,
        };
        rec.save(helper)?;
        Ok(rec)
    }

    /// The pin this record makes of `helper`.
    #[must_use]
    pub fn pinned(&self, helper: &Path) -> PinnedFile {
        PinnedFile {
            path: helper.to_path_buf(),
            sha256: self.sha256.clone(),
            stamp: Some(self.stamp),
        }
    }

    /// The record of `helper`, `None` if there is none.
    pub fn load(helper: &Path) -> Result<Option<Self>, PinError> {
        let p = helper_record_path(helper);
        let raw = match std::fs::read(&p) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(PinError::Io(format!("reading {}: {e}", p.display()))),
        };
        let rec: Self = serde_json::from_slice(&raw)
            .map_err(|e| PinError::Io(format!("{}: {e}", p.display())))?;
        if rec.schema != HELPER_RECORD_SCHEMA {
            return Err(PinError::Io(format!(
                "{}: schema {} (want {HELPER_RECORD_SCHEMA}); reinstall the helper",
                p.display(),
                rec.schema
            )));
        }
        Ok(Some(rec))
    }

    /// Write the record next to `helper` (temp file + rename).
    pub fn save(&self, helper: &Path) -> Result<(), PinError> {
        let p = helper_record_path(helper);
        let tmp = p.with_extension(format!("{}.tmp", std::process::id()));
        let io = |e: std::io::Error| PinError::Io(format!("writing {}: {e}", p.display()));
        let res = (|| {
            let mut f = File::create(&tmp).map_err(io)?;
            let mut json = serde_json::to_vec_pretty(self).expect("record serializes");
            json.push(b'\n');
            f.write_all(&json).and_then(|()| f.sync_all()).map_err(io)?;
            std::fs::rename(&tmp, &p).map_err(io)
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helper_proto::ROOT_DRIVE_ID;

    #[test]
    fn disk_with_stamp_only_must_be_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rootfs.ext4");
        std::fs::write(&p, b"image").unwrap();
        let stamp = FileStamp::of(&std::fs::metadata(&p).unwrap());
        let mut d = DiskSpec {
            block_id: ROOT_DRIVE_ID.into(),
            path: p.clone(),
            read_only: true,
            sha256: None,
            stamp: Some(stamp),
        };
        assert!(open_disk(&d).unwrap().is_some());
        std::fs::rename(&p, tmp.path().join("old")).unwrap();
        std::fs::write(&p, b"image").unwrap();
        assert!(matches!(open_disk(&d), Err(PinError::Mismatch(_))));
        d.stamp = None;
        assert!(open_disk(&d).unwrap().is_none(), "no pin: by path");
    }

    #[test]
    fn disk_with_digest_is_rehashed_on_a_stamp_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rootfs.ext4");
        std::fs::write(&p, b"image").unwrap();
        let (sha, stamp) = stamp::record(&p).unwrap();
        let mut d = DiskSpec {
            block_id: ROOT_DRIVE_ID.into(),
            path: p.clone(),
            read_only: true,
            sha256: Some(sha),
            stamp: Some(stamp),
        };
        let f = open_disk(&d).unwrap().unwrap();
        assert_eq!(std::fs::read(fd_path(&f)).unwrap(), b"image");
        // Stale stamp, same content: accepted after a re-hash.
        d.stamp = Some(FileStamp {
            ctime_ns: 1,
            ..stamp
        });
        assert!(open_disk(&d).unwrap().is_some());
        // Swapped content: refused.
        std::fs::write(&p, b"evil!").unwrap();
        assert!(matches!(open_disk(&d), Err(PinError::Mismatch(_))));
    }

    #[test]
    fn helper_record_round_trip_and_tamper() {
        let tmp = tempfile::tempdir().unwrap();
        let h = tmp.path().join("sandcrab-krun");
        std::fs::write(&h, b"#!helper").unwrap();
        assert!(HelperRecord::load(&h).unwrap().is_none());
        let rec = HelperRecord::record(&h).unwrap();
        assert_eq!(
            helper_record_path(&h),
            tmp.path().join("sandcrab-krun.pin.json")
        );
        assert_eq!(HelperRecord::load(&h).unwrap(), Some(rec.clone()));
        let pin = PinnedFile {
            path: h.clone(),
            sha256: rec.sha256.clone(),
            stamp: Some(rec.stamp),
        };
        assert_eq!(open_pinned(&pin).unwrap().how, stamp::Verification::Stamp);
        std::fs::write(&h, b"#!evil!!").unwrap();
        assert!(matches!(open_pinned(&pin), Err(PinError::Mismatch(_))));
    }
}
