//! Workspace drives (design §5): the host directory stays the source of truth.
//!
//! - **Attach** ([`build_image`]): pack the host tree into a `sandcrab-tar-v1`-style tar
//!   ([`pack_dir`]: symlinks stored not followed, special files skipped, setuid/setgid
//!   dropped, ownership normalised to the guest user) and `mke2fs -d` it into an ext4 image.
//!   Read-only images are cached per (generation, tree fingerprint, owner)
//!   ([`cached_ro_image`]) and shared by every sandbox that mounts that generation.
//! - **Sync-back** ([`receive_export`] + [`commit`]): the guest streams the rw tree back over
//!   vsock; [`sandcrab_core::archive::unpack_safe`] writes it into an empty staging dir, then
//!   it is committed: a `ManagedDir` by an atomic directory swap, a `HostDir` by an in-place
//!   mirror-apply (non-atomic; documented).
//!
//! Everything here is blocking; callers use `spawn_blocking`.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sandcrab_core::archive::{UnpackLimits, UnpackStats, unpack_safe};
use sandcrab_core::{Result, SandboxError, WorkspaceBacking};
use sandcrab_protocol::init::archive::{DIR_PERMISSION_MASK, PERMISSION_MASK};
use sha2::{Digest, Sha256};

use crate::rootfs::mkfs::{self, MkfsParams, MkfsStrategy};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// What [`pack_dir`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackStats {
    pub dirs: u64,
    pub files: u64,
    pub symlinks: u64,
    /// File content bytes.
    pub bytes: u64,
    /// Sockets, FIFOs, devices and entries on another filesystem.
    pub skipped: u64,
}

impl PackStats {
    #[must_use]
    pub fn entries(&self) -> u64 {
        self.dirs + self.files + self.symlinks
    }
}

/// Pack the tree under `root` (not included itself) into a tar on `w`, with every entry
/// owned by `owner`. Never follows symlinks; stays on `root`'s filesystem; deterministic
/// (byte-ordered pre-order walk).
pub fn pack_dir<W: Write>(root: &Path, owner: (u32, u32), w: W) -> io::Result<PackStats> {
    let root_md = fs::symlink_metadata(root)?;
    if !root_md.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", root.display()),
        ));
    }
    let dev = root_md.dev();
    let mut stats = PackStats::default();
    let mut tar = tar::Builder::new(w);
    tar.mode(tar::HeaderMode::Complete);
    let mut stack: Vec<(PathBuf, PathBuf)> = Vec::new();
    push_children(root, Path::new(""), &mut stack)?;
    while let Some((rel, abs)) = stack.pop() {
        let md = match fs::symlink_metadata(&abs) {
            Ok(md) => md,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if md.dev() != dev {
            stats.skipped += 1;
            continue;
        }
        let ft = md.file_type();
        let mut h = tar::Header::new_gnu();
        h.set_uid(u64::from(owner.0));
        h.set_gid(u64::from(owner.1));
        h.set_mtime(u64::try_from(md.mtime()).unwrap_or(0));
        if ft.is_dir() {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mode(md.mode() & DIR_PERMISSION_MASK);
            h.set_size(0);
            tar.append_data(&mut h, &rel, io::empty())?;
            stats.dirs += 1;
            match push_children(&abs, &rel, &mut stack) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        } else if ft.is_symlink() {
            let target = match fs::read_link(&abs) {
                Ok(t) => t,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_mode(0o777);
            h.set_size(0);
            tar.append_link(&mut h, &rel, &target)?;
            stats.symlinks += 1;
        } else if ft.is_file() {
            let file = match File::open(&abs) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let size = md.len();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(md.mode() & PERMISSION_MASK);
            h.set_size(size);
            // Exactly `size` bytes even if the file changes underneath (zero-pad / truncate).
            let reader = file.take(size).chain(io::repeat(0)).take(size);
            tar.append_data(&mut h, &rel, reader)?;
            stats.files += 1;
            stats.bytes += size;
        } else {
            stats.skipped += 1;
        }
    }
    tar.into_inner()?.flush()?;
    Ok(stats)
}

fn push_children(abs: &Path, rel: &Path, stack: &mut Vec<(PathBuf, PathBuf)>) -> io::Result<()> {
    let mut names: Vec<_> = fs::read_dir(abs)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<io::Result<_>>()?;
    names.sort();
    for name in names.into_iter().rev() {
        stack.push((rel.join(&name), abs.join(&name)));
    }
    Ok(())
}

/// Cheap change detector for a tree: entry count, total size, newest mtime/ctime (ns),
/// hashed. Catches out-of-band edits of a `HostDir` between generations.
pub fn fingerprint(root: &Path) -> io::Result<String> {
    let mut count = 0u64;
    let mut size = 0u64;
    let mut max_m = i128::MIN;
    let mut max_c = i128::MIN;
    let dev = fs::symlink_metadata(root)?.dev();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let md = fs::symlink_metadata(&dir)?;
        max_m = max_m.max(i128::from(md.mtime()) * 1_000_000_000 + i128::from(md.mtime_nsec()));
        max_c = max_c.max(i128::from(md.ctime()) * 1_000_000_000 + i128::from(md.ctime_nsec()));
        for e in fs::read_dir(&dir)? {
            let e = e?;
            let md = match fs::symlink_metadata(e.path()) {
                Ok(md) => md,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            if md.dev() != dev {
                continue;
            }
            count += 1;
            size += md.len();
            max_m = max_m.max(i128::from(md.mtime()) * 1_000_000_000 + i128::from(md.mtime_nsec()));
            max_c = max_c.max(i128::from(md.ctime()) * 1_000_000_000 + i128::from(md.ctime_nsec()));
            if md.is_dir() {
                stack.push(e.path());
            }
        }
    }
    let mut h = Sha256::new();
    h.update(format!("{count}:{size}:{max_m}:{max_c}"));
    Ok(crate::hash::to_hex(&h.finalize())[..16].to_owned())
}

/// Image geometry for a workspace drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSize {
    pub bytes: u64,
    pub inodes: u64,
}

/// Read-write: `max(2 × content, content + 1 GiB, quota)` (sparse, so the headroom is free
/// until used), inodes `max(bytes / 16 KiB, 2 × entries + 4096)`. Read-only: the rootfs
/// policy (`content × 1.15 + 64 MiB`, no headroom). Both rounded up to 64 MiB.
#[must_use]
pub fn image_size(
    content_bytes: u64,
    entries: u64,
    read_only: bool,
    quota_mb: Option<u64>,
) -> ImageSize {
    if read_only {
        return ImageSize {
            bytes: crate::rootfs::image_size(content_bytes),
            inodes: crate::rootfs::inode_count(entries),
        };
    }
    let bytes = (2 * content_bytes)
        .max(content_bytes + GIB)
        .max(quota_mb.unwrap_or(0) * MIB)
        .div_ceil(64 * MIB)
        * (64 * MIB);
    ImageSize {
        bytes,
        inodes: (bytes / (16 * 1024)).max(2 * entries + 4096),
    }
}

/// Options of one workspace image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageOpts<'a> {
    /// ext4 label (the guest's drive lookup fallback).
    pub label: &'a str,
    /// Owner of the image root and every entry (the guest user).
    pub owner: (u32, u32),
    /// Read-only image: no journal, no free-space headroom.
    pub read_only: bool,
    pub quota_mb: Option<u64>,
}

/// Build `img` from host dir `root`, using `work_dir` (which must contain `img`) for the
/// intermediate tar.
pub fn build_image(
    strategy: MkfsStrategy,
    root: &Path,
    work_dir: &Path,
    img: &Path,
    opts: &ImageOpts<'_>,
) -> Result<PackStats> {
    let ImageOpts {
        label,
        owner,
        read_only,
        quota_mb,
    } = *opts;
    let stem = img
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tar_path = work_dir.join(format!("{stem}.tar"));
    let res = (|| {
        let stats = {
            let f = File::create(&tar_path).map_err(|e| io_at(&tar_path, e))?;
            let mut w = BufWriter::with_capacity(1 << 20, f);
            let stats = pack_dir(root, owner, &mut w).map_err(|e| io_at(root, e))?;
            w.into_inner()
                .map_err(|e| io_at(&tar_path, e.into_error()))?
                .sync_data()
                .map_err(|e| io_at(&tar_path, e))?;
            stats
        };
        let content = fs::metadata(&tar_path)
            .map_err(|e| io_at(&tar_path, e))?
            .len();
        let size = image_size(content, stats.entries(), read_only, quota_mb);
        let params = MkfsParams {
            label: label.to_owned(),
            uuid: uuid::Uuid::new_v4().to_string(),
            size_bytes: size.bytes,
            inodes: size.inodes,
            journal: !read_only,
            root_owner: Some(owner),
        };
        mkfs::mkfs_from_tar(strategy, work_dir, &tar_path, img, &params).map_err(|e| {
            let _ = fs::remove_file(img);
            SandboxError::Workspace(format!(
                "building workspace image from {}: {e}",
                root.display()
            ))
        })?;
        Ok(stats)
    })();
    let _ = fs::remove_file(&tar_path);
    res
}

/// Shared read-only image for (`workspace_id`, `generation`, tree fingerprint, `owner`)
/// under `cache_dir/<workspace_id>/`. Builds it on a miss (temp dir + atomic rename) and
/// prunes images of older keys. Returns `(image, cache_hit)`.
pub fn cached_ro_image(
    strategy: MkfsStrategy,
    cache_dir: &Path,
    workspace_id: &str,
    generation: u64,
    root: &Path,
    owner: (u32, u32),
    label: &str,
) -> Result<(PathBuf, bool)> {
    let dir = cache_dir.join(workspace_id);
    create_private_dir_all(&dir)?;
    let fp = fingerprint(root).map_err(|e| io_at(root, e))?;
    let name = format!("ro-{generation}-{fp}-{}-{}-{label}.ext4", owner.0, owner.1);
    let img = dir.join(&name);
    if img.is_file() {
        return Ok((img, true));
    }
    let tmp = mkfs::tempdir_in(&dir, "build").map_err(|e| backend(format!("{e}")))?;
    let tmp_img = tmp.join(&name);
    let opts = ImageOpts {
        label,
        owner,
        read_only: true,
        quota_mb: None,
    };
    let res = build_image(strategy, root, &tmp, &tmp_img, &opts).and_then(|_| {
        fs::set_permissions(&tmp_img, fs::Permissions::from_mode(0o444))
            .and_then(|()| fs::rename(&tmp_img, &img))
            .map_err(|e| io_at(&img, e))
    });
    let _ = fs::remove_dir_all(&tmp);
    res?;
    // Prune older keys; sandboxes still running keep their open file (unlink is safe).
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let n = e.file_name();
            let n = n.to_string_lossy();
            if n.starts_with("ro-") && n.ends_with(".ext4") && *n != *name {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    Ok((img, false))
}

/// Per-sandbox scratch drive: sparse copy of a cached pre-formatted blank image
/// (`cache/blank/<mib>.ext4`, created once). Falls back to formatting in place.
pub fn scratch_drive(cache_root: &Path, size_mib: u64, dest: &Path) -> Result<()> {
    let blank_dir = cache_root.join("blank");
    let blank = blank_dir.join(format!("{size_mib}.ext4"));
    if !blank.is_file() {
        create_private_dir_all(&blank_dir)?;
        let tmp = blank_dir.join(format!(".{size_mib}.ext4.{}.part", std::process::id()));
        crate::rootfs::create_scratch(&tmp, size_mib).map_err(|e| backend(format!("{e}")))?;
        fs::rename(&tmp, &blank).map_err(|e| io_at(&blank, e))?;
    }
    match sparse_copy(&blank, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::debug!(error = %e, "sparse copy failed; formatting scratch in place");
            let _ = fs::remove_file(dest);
            crate::rootfs::create_scratch(dest, size_mib)
                .map(drop)
                .map_err(|e| backend(format!("{e}")))
        }
    }
}

/// Copy only the data extents of `src` (`SEEK_DATA`/`SEEK_HOLE`), keeping holes.
#[cfg(target_os = "linux")]
pub fn sparse_copy(src: &Path, dest: &Path) -> io::Result<()> {
    use std::os::unix::fs::FileExt;

    use rustix::fs::{SeekFrom, seek};

    let input = File::open(src)?;
    let len = input.metadata()?.len();
    let out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    out.set_len(len)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    while pos < len {
        let data = match seek(&input, SeekFrom::Data(pos)) {
            Ok(d) => d,
            Err(rustix::io::Errno::NXIO) => break, // only a hole remains
            Err(e) => return Err(e.into()),
        };
        let hole = seek(&input, SeekFrom::Hole(data))?.min(len);
        let mut off = data;
        while off < hole {
            let want = usize::try_from((hole - off).min(buf.len() as u64)).unwrap_or(buf.len());
            let n = input.read_at(&mut buf[..want], off)?;
            if n == 0 {
                break;
            }
            out.write_all_at(&buf[..n], off)?;
            off += n as u64;
        }
        pos = hole;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn sparse_copy(src: &Path, dest: &Path) -> io::Result<()> {
    fs::copy(src, dest).map(drop)
}

/// Read a guest export (raw tar until EOF) from `r` into the fresh, empty `staging` dir.
pub fn receive_export<R: Read>(r: R, staging: &Path, limits: &UnpackLimits) -> Result<UnpackStats> {
    unpack_safe(r, staging, limits)
}

/// Where to stage the sync-back of a workspace so that [`commit`] is cheap: next to a
/// managed data dir (same filesystem → atomic swap), else under the sandbox run dir.
#[must_use]
pub fn staging_dir(
    backing: &WorkspaceBacking,
    host_path: &Path,
    run_dir: &Path,
    tag: &str,
) -> PathBuf {
    match backing {
        WorkspaceBacking::ManagedDir(_) => {
            let parent = host_path.parent().unwrap_or(run_dir);
            parent.join(format!(".sc-staging-{tag}"))
        }
        _ => run_dir.join(format!("staging-{tag}")),
    }
}

/// Create an empty private staging dir (removing a stale one first).
pub fn fresh_staging(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        fs::remove_dir_all(path).map_err(|e| io_at(path, e))?;
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|e| io_at(path, e))
}

/// Commit `staging` as the new content of `target`.
///
/// - `ManagedDir`: swap the directories (`RENAME_EXCHANGE` where available, else two
///   renames), keeping `target`'s mode, then delete the old tree.
/// - `HostDir`: mirror-apply in place (entries absent from staging are deleted; files and
///   symlinks are replaced by rename, or copied across filesystems). Not atomic: a crash
///   mid-apply leaves a mix, which is the documented V1 trade-off for user directories.
pub fn commit(backing: &WorkspaceBacking, staging: &Path, target: &Path) -> Result<()> {
    match backing {
        WorkspaceBacking::ManagedDir(_) => swap_dirs(staging, target),
        WorkspaceBacking::HostDir(_) => {
            mirror_apply(staging, target).map_err(|e| {
                SandboxError::Workspace(format!(
                    "applying sync-back to {} failed midway: {e}",
                    target.display()
                ))
            })?;
            let _ = fs::remove_dir_all(staging);
            Ok(())
        }
        WorkspaceBacking::BlockVolume(_) => Err(SandboxError::Unimplemented(
            "sync-back of a block volume (its image is the data)",
        )),
    }
}

fn swap_dirs(staging: &Path, target: &Path) -> Result<()> {
    let mode = fs::metadata(target).map_err(|e| io_at(target, e))?.mode() & 0o7777;
    fs::set_permissions(staging, fs::Permissions::from_mode(mode))
        .map_err(|e| io_at(staging, e))?;
    if exchange(staging, target).is_ok() {
        // `staging` now holds the old tree.
        let _ = fs::remove_dir_all(staging);
        return Ok(());
    }
    let old = target.with_file_name(format!(
        ".sc-old-{}",
        target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let _ = fs::remove_dir_all(&old);
    fs::rename(target, &old).map_err(|e| io_at(target, e))?;
    if let Err(e) = fs::rename(staging, target) {
        let _ = fs::rename(&old, target);
        return Err(io_at(target, e));
    }
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

#[cfg(target_os = "linux")]
fn exchange(a: &Path, b: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, a, CWD, b, RenameFlags::EXCHANGE).map_err(io::Error::from)
}

#[cfg(not(target_os = "linux"))]
fn exchange(_a: &Path, _b: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// Make `target` mirror `staging`. Never follows symlinks in either tree: every lookup is
/// `lstat`, a symlink or file in the way of a directory is removed first, and recursion only
/// enters real directories.
pub fn mirror_apply(staging: &Path, target: &Path) -> io::Result<()> {
    let names: BTreeSet<_> = fs::read_dir(staging)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<io::Result<_>>()?;
    // 1. Delete what the guest deleted.
    for e in fs::read_dir(target)? {
        let e = e?;
        if !names.contains(&e.file_name()) {
            remove_any(&e.path())?;
        }
    }
    // 2. Add / replace.
    for name in &names {
        let s = staging.join(name);
        let t = target.join(name);
        let smd = fs::symlink_metadata(&s)?;
        let tmd = fs::symlink_metadata(&t).ok();
        if smd.is_dir() {
            match tmd {
                Some(md) if md.is_dir() => {}
                Some(_) => {
                    fs::remove_file(&t)?;
                    fs::create_dir(&t)?;
                }
                None => fs::create_dir(&t)?,
            }
            mirror_apply(&s, &t)?;
            fs::set_permissions(
                &t,
                fs::Permissions::from_mode(smd.mode() & DIR_PERMISSION_MASK),
            )?;
        } else {
            if tmd.is_some_and(|md| md.is_dir()) {
                fs::remove_dir_all(&t)?;
            }
            match fs::rename(&s, &t) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(18) /* EXDEV */ => copy_entry(&s, &t, &smd)?,
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

fn copy_entry(s: &Path, t: &Path, smd: &fs::Metadata) -> io::Result<()> {
    let tmp = t.with_file_name(format!(
        ".sc-tmp-{}",
        t.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let _ = fs::remove_file(&tmp);
    if smd.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(s)?, &tmp)?;
    } else {
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        io::copy(&mut File::open(s)?, &mut out)?;
        out.set_permissions(fs::Permissions::from_mode(smd.mode() & PERMISSION_MASK))?;
        if let Ok(m) = smd.modified() {
            let _ = out.set_modified(m);
        }
    }
    fs::rename(&tmp, t)
}

fn remove_any(p: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(p)?;
    if md.is_dir() {
        fs::remove_dir_all(p)
    } else {
        fs::remove_file(p)
    }
}

pub(crate) fn create_private_dir_all(p: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
        .map_err(|e| io_at(p, e))
}

/// Default idle timeout while reading an export stream.
pub const EXPORT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn io_at(path: &Path, e: io::Error) -> SandboxError {
    SandboxError::Io(io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

fn backend(message: String) -> SandboxError {
    SandboxError::Backend {
        backend: "firecracker".into(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::process::Command;

    use super::*;

    fn tree(root: &Path) {
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(root.join("README.md"), "hello\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src/nested/run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(
            root.join("src/nested/run.sh"),
            fs::Permissions::from_mode(0o4755),
        )
        .unwrap();
        symlink("/etc/passwd", root.join("escape")).unwrap();
        std::os::unix::net::UnixListener::bind(root.join("sock")).unwrap();
    }

    #[test]
    fn pack_dir_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        tree(&root);
        let mut buf = Vec::new();
        let stats = pack_dir(&root, (1000, 1001), &mut buf).unwrap();
        assert_eq!(
            (stats.dirs, stats.files, stats.symlinks, stats.skipped),
            (2, 3, 1, 1)
        );
        let mut ar = tar::Archive::new(&buf[..]);
        let mut seen = Vec::new();
        for e in ar.entries().unwrap() {
            let e = e.unwrap();
            let h = e.header();
            assert_eq!((h.uid().unwrap(), h.gid().unwrap()), (1000, 1001));
            let p = e.path().unwrap().into_owned();
            if p == Path::new("src/nested/run.sh") {
                assert_eq!(h.mode().unwrap(), 0o755, "setuid dropped");
            }
            if p == Path::new("escape") {
                assert_eq!(e.link_name().unwrap().unwrap(), Path::new("/etc/passwd"));
            }
            seen.push(p.display().to_string());
        }
        assert_eq!(
            seen,
            [
                "README.md",
                "escape",
                "src",
                "src/main.rs",
                "src/nested",
                "src/nested/run.sh"
            ]
        );
        // Round-trips through the host's hostile-archive unpacker.
        let staging = dir.path().join("staging");
        fs::create_dir(&staging).unwrap();
        receive_export(&buf[..], &staging, &UnpackLimits::default()).unwrap();
        assert_eq!(
            fs::read_to_string(staging.join("src/main.rs")).unwrap(),
            "fn main() {}\n"
        );
        assert!(pack_dir(&root.join("README.md"), (0, 0), Vec::new()).is_err());
    }

    #[test]
    fn fingerprint_tracks_changes() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let a = fingerprint(dir.path()).unwrap();
        assert_eq!(a, fingerprint(dir.path()).unwrap());
        fs::write(dir.path().join("src/new.txt"), "x").unwrap();
        assert_ne!(a, fingerprint(dir.path()).unwrap());
    }

    #[test]
    fn size_policy() {
        let rw = image_size(10 * MIB, 100, false, None);
        assert_eq!(rw.bytes, 10 * MIB + GIB + 54 * MIB); // 1034 MiB → 1088 MiB (64-aligned)
        assert_eq!(rw.inodes, rw.bytes / 16384);
        let big = image_size(3 * GIB, 10, false, None);
        assert_eq!(big.bytes, 6 * GIB);
        let quota = image_size(0, 0, false, Some(4096));
        assert_eq!(quota.bytes, 4 * GIB);
        let many = image_size(0, 1_000_000, false, None);
        assert_eq!(many.inodes, 2_004_096);
        let ro = image_size(100 * MIB, 10, true, None);
        assert_eq!(ro.bytes, 192 * MIB);
    }

    fn debugfs(img: &Path, cmd: &str) -> String {
        let out = Command::new("debugfs")
            .args(["-R", cmd])
            .arg(img)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn have(tool: &str) -> bool {
        Command::new(tool).arg("-V").output().is_ok()
    }

    /// Builds real ext4 images and inspects them with debugfs (skipped without e2fsprogs).
    #[test]
    fn image_build_and_ro_cache() {
        if !have("mke2fs") || !have("debugfs") {
            eprintln!("e2fsprogs not installed; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let Ok(strategy) = mkfs::detect(dir.path()) else {
            eprintln!("no mkfs strategy; skipping");
            return;
        };
        let root = dir.path().join("ws");
        tree(&root);
        let img = dir.path().join("ws-0.ext4");
        let opts = ImageOpts {
            label: "ws0",
            owner: (1000, 1000),
            read_only: false,
            quota_mb: None,
        };
        let stats = build_image(strategy, &root, dir.path(), &img, &opts).unwrap();
        assert_eq!(stats.files, 3);
        assert!(!dir.path().join("ws-0.ext4.tar").exists(), "tar removed");
        let md = fs::metadata(&img).unwrap();
        assert_eq!(md.len() % (64 * MIB), 0);
        assert!(md.blocks() * 512 < md.len() / 4, "image is sparse");
        assert_eq!(debugfs(&img, "cat /src/main.rs"), "fn main() {}\n");
        let stat_root = debugfs(&img, "stat /");
        assert!(stat_root.contains("User:  1000"), "{stat_root}");
        let stat = debugfs(&img, "stat /escape");
        assert!(stat.contains("Type: symlink"), "{stat}");
        assert!(
            debugfs(&img, "stats").contains("has_journal"),
            "rw image journaled"
        );

        let cache = dir.path().join("ro");
        let (a, hit) =
            cached_ro_image(strategy, &cache, "wsid", 3, &root, (1000, 1000), "ws1").unwrap();
        assert!(!hit);
        assert!(!debugfs(&a, "stats").contains("has_journal"));
        let (b, hit) =
            cached_ro_image(strategy, &cache, "wsid", 3, &root, (1000, 1000), "ws1").unwrap();
        assert!(hit && a == b);
        let (c, hit) =
            cached_ro_image(strategy, &cache, "wsid", 4, &root, (1000, 1000), "ws1").unwrap();
        assert!(!hit && c != a);
        assert!(!a.exists(), "older generation pruned");

        let empty = dir.path().join("empty");
        fs::create_dir(&empty).unwrap();
        let img = dir.path().join("empty.ext4");
        let opts = ImageOpts {
            label: "ws2",
            owner: (0, 0),
            ..opts
        };
        build_image(strategy, &empty, dir.path(), &img, &opts).unwrap();
        assert!(debugfs(&img, "ls -l /").contains("lost+found"));
    }

    #[test]
    fn scratch_from_blank_is_sparse() {
        if !have("mke2fs") {
            eprintln!("mke2fs not installed; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("scratch.ext4");
        scratch_drive(dir.path(), 512, &dest).unwrap();
        assert!(dir.path().join("blank/512.ext4").is_file());
        let md = fs::metadata(&dest).unwrap();
        assert_eq!(md.len(), 512 * MIB);
        assert!(md.blocks() * 512 < 64 * MIB);
        let a = fs::read(dir.path().join("blank/512.ext4")).unwrap();
        assert!(a == fs::read(&dest).unwrap(), "copy is byte-identical");
        let dest2 = dir.path().join("scratch2.ext4");
        scratch_drive(dir.path(), 512, &dest2).unwrap();
    }

    #[test]
    fn mirror_apply_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("host");
        let staging = dir.path().join("staging");
        let outside = dir.path().join("outside");
        fs::create_dir_all(target.join("gone/deep")).unwrap();
        fs::create_dir_all(target.join("keep")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(target.join("gone/deep/x"), "x").unwrap();
        fs::write(target.join("keep/old.txt"), "old").unwrap();
        fs::write(target.join("file-becomes-dir"), "f").unwrap();
        fs::create_dir(target.join("dir-becomes-file")).unwrap();
        // A host symlink where the guest now has a directory must be replaced, never
        // followed.
        symlink(&outside, target.join("link")).unwrap();

        fs::create_dir_all(staging.join("keep")).unwrap();
        fs::write(staging.join("keep/old.txt"), "new").unwrap();
        fs::write(staging.join("keep/added.txt"), "added").unwrap();
        fs::create_dir(staging.join("file-becomes-dir")).unwrap();
        fs::write(staging.join("dir-becomes-file"), "now a file").unwrap();
        fs::create_dir(staging.join("link")).unwrap();
        fs::write(staging.join("link/inside"), "safe").unwrap();
        symlink("../elsewhere", staging.join("guest-link")).unwrap();

        commit(
            &WorkspaceBacking::HostDir(target.clone()),
            &staging,
            &target,
        )
        .unwrap();
        assert!(!target.join("gone").exists());
        assert_eq!(
            fs::read_to_string(target.join("keep/old.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(target.join("keep/added.txt")).unwrap(),
            "added"
        );
        assert!(target.join("file-becomes-dir").is_dir());
        assert_eq!(
            fs::read_to_string(target.join("dir-becomes-file")).unwrap(),
            "now a file"
        );
        assert!(fs::symlink_metadata(target.join("link")).unwrap().is_dir());
        assert_eq!(
            fs::read_dir(&outside).unwrap().count(),
            0,
            "symlink not followed"
        );
        assert_eq!(
            fs::read_link(target.join("guest-link")).unwrap(),
            Path::new("../elsewhere")
        );
        assert!(!staging.exists());
    }

    #[test]
    fn managed_commit_swaps() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(data.join("old"), "1").unwrap();
        let staging = staging_dir(
            &WorkspaceBacking::ManagedDir(data.clone()),
            &data,
            Path::new("/nonexistent"),
            "sb-0",
        );
        assert_eq!(staging, dir.path().join(".sc-staging-sb-0"));
        fresh_staging(&staging).unwrap();
        fs::write(staging.join("new"), "2").unwrap();
        commit(&WorkspaceBacking::ManagedDir(data.clone()), &staging, &data).unwrap();
        assert!(!data.join("old").exists());
        assert_eq!(fs::read_to_string(data.join("new")).unwrap(), "2");
        assert_eq!(fs::metadata(&data).unwrap().mode() & 0o777, 0o700);
        assert!(!staging.exists());
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, ["data"]);
    }
}
