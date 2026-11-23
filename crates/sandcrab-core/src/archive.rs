//! Host side of workspace sync-back: unpack an untrusted `sandcrab-tar-v1` archive (see
//! [`sandcrab_protocol::init::archive`]) into a fresh staging directory.
//!
//! The archive comes from a guest and is **hostile input**. [`unpack_safe`] only ever creates
//! entries strictly below `staging` and rejects (whole archive, no partial "best effort"):
//! - absolute paths, `..`, NUL, over-long paths or over-deep trees;
//! - hard links, character / block devices, FIFOs and any other entry type besides
//!   directory, regular file and symlink;
//! - setuid / setgid bits;
//!
//! Modes are **masked**, never applied verbatim: the guest must not be able to make host
//! directories world-writable or lock the owner out. Every mode is `& 0o777 & !umask`
//! ([`UnpackLimits::umask`], default `0o022`; sticky is dropped too), then files get `u+rw`
//! and directories `u+rwx`.
//! - writing through a symlink: every parent component must be a real directory created by
//!   this unpack (checked with `lstat`, never followed), and files are created with
//!   `O_CREAT | O_EXCL`, which refuses an existing symlink;
//! - duplicate entries (a second entry for an existing path, except a repeated directory);
//! - more than [`UnpackLimits`] entries / bytes, or a stream that ends without the
//!   end-of-archive marker (a guest that crashed mid-export).
//!
//! **Disk bound.** A guest can make the archive far bigger than its drive (a sparse
//! `truncate -s 15G f`, or hard links, each stored as an independent file), so the byte caps
//! must come from the drive, not from a fixed constant: backends pass
//! [`UnpackLimits::for_capacity`] / [`UnpackLimits::bounded_by`] with the workspace drive
//! size (and an optional host quota). Runs of zeros are written as holes, so host disk use
//! tracks the real data even for legitimately sparse files.
//!
//! Symlinks are recreated verbatim (their targets may point anywhere); consumers of the
//! staging tree must not follow them, exactly as for a live `HostDir` workspace.
//!
//! The function is blocking; call it from `spawn_blocking`.

use std::{
    fs,
    io::{self, Read},
    os::unix::{
        ffi::OsStrExt,
        fs::{PermissionsExt, symlink},
    },
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::{Result, SafeRelPath, SandboxError};

/// Resource caps for [`unpack_safe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnpackLimits {
    /// Max number of entries (directories + files + symlinks).
    pub max_entries: u64,
    /// Max total file content bytes.
    pub max_total_bytes: u64,
    /// Max bytes of one file.
    pub max_file_bytes: u64,
    /// Max entry path length in bytes.
    pub max_path_len: usize,
    /// Max path depth (components).
    pub max_depth: usize,
    /// Permission bits removed from every guest-supplied mode.
    pub umask: u32,
}

/// Bytes allowed on top of a drive's capacity (tar content never exceeds the filesystem
/// size, so this is only a safety margin for rounding).
pub const CAPACITY_SLACK_BYTES: u64 = 16 * 1024 * 1024;

impl UnpackLimits {
    /// [`Self::default`] bounded by a drive of `capacity` bytes (see [`Self::bounded_by`]).
    #[must_use]
    pub fn for_capacity(capacity: u64) -> Self {
        Self::default().bounded_by(capacity)
    }

    /// Tighten `self` for a drive of `capacity` bytes: total content ≤ `capacity` +
    /// [`CAPACITY_SLACK_BYTES`], one file ≤ that total, and at most one entry per 512 bytes of
    /// capacity (never fewer than 65 536). Existing tighter caps are kept.
    #[must_use]
    pub fn bounded_by(&self, capacity: u64) -> Self {
        let total = self
            .max_total_bytes
            .min(capacity.saturating_add(CAPACITY_SLACK_BYTES));
        Self {
            max_total_bytes: total,
            max_file_bytes: self.max_file_bytes.min(total),
            max_entries: self.max_entries.min((capacity / 512).max(65_536)),
            ..self.clone()
        }
    }
}

impl Default for UnpackLimits {
    fn default() -> Self {
        Self {
            max_entries: 1_000_000,
            max_total_bytes: 64 * 1024 * 1024 * 1024,
            max_file_bytes: 16 * 1024 * 1024 * 1024,
            max_path_len: 4096,
            max_depth: 256,
            umask: 0o022,
        }
    }
}

/// What [`unpack_safe`] created.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnpackStats {
    pub dirs: u64,
    pub files: u64,
    pub symlinks: u64,
    pub bytes: u64,
}

fn reject(msg: impl Into<String>) -> SandboxError {
    SandboxError::Workspace(format!("archive rejected: {}", msg.into()))
}

fn escape(msg: impl Into<String>) -> SandboxError {
    SandboxError::PathEscape(format!("archive entry {}", msg.into()))
}

/// Tracks whether the underlying reader hit EOF, to tell "end-of-archive marker seen" from
/// "stream ended early" (the `tar` crate treats both as the end of the entry iterator).
struct EofTracker<R> {
    inner: R,
    eof: bool,
}

impl<R: Read> Read for EofTracker<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 && !buf.is_empty() {
            self.eof = true;
        }
        Ok(n)
    }
}

/// Unpack `r` into `staging`, which must be an existing, empty directory.
///
/// On error the staging directory is left partially filled; the caller discards it.
pub fn unpack_safe(r: impl Read, staging: &Path, limits: &UnpackLimits) -> Result<UnpackStats> {
    let md = fs::symlink_metadata(staging)?;
    if !md.is_dir() {
        return Err(SandboxError::InvalidConfig(format!(
            "staging `{}` is not a directory",
            staging.display()
        )));
    }
    if fs::read_dir(staging)?.next().is_some() {
        return Err(SandboxError::InvalidConfig(format!(
            "staging `{}` is not empty",
            staging.display()
        )));
    }

    let mut archive = tar::Archive::new(EofTracker {
        inner: r,
        eof: false,
    });
    let mut stats = UnpackStats::default();
    let mut entries_seen = 0u64;
    // Directory modes are applied last (a read-only directory would block its own contents).
    let mut dir_modes: Vec<(PathBuf, u32, Option<SystemTime>)> = Vec::new();

    for entry in archive
        .entries()
        .map_err(|e| reject(format!("unreadable archive: {e}")))?
    {
        let mut entry = entry.map_err(|e| reject(format!("corrupt entry: {e}")))?;
        entries_seen += 1;
        if entries_seen > limits.max_entries {
            return Err(reject(format!("more than {} entries", limits.max_entries)));
        }

        let raw = entry.path_bytes().into_owned();
        let rel = check_path(&raw, limits)?;
        let header = entry.header();
        let ty = header.entry_type();
        let mode = header
            .mode()
            .map_err(|e| reject(format!("bad mode on `{}`: {e}", rel.display())))?;
        if mode & 0o6000 != 0 {
            return Err(reject(format!("setuid/setgid bit on `{}`", rel.display())));
        }
        let mtime = header
            .mtime()
            .ok()
            .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s));

        let dest = staging.join(&rel);
        ensure_parents(staging, &rel)?;

        match ty {
            tar::EntryType::Directory => {
                match fs::symlink_metadata(&dest) {
                    Ok(m) if m.is_dir() => {}
                    Ok(_) => return Err(reject(format!("duplicate entry `{}`", rel.display()))),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        fs::create_dir(&dest)?;
                        stats.dirs += 1;
                    }
                    Err(e) => return Err(e.into()),
                }
                dir_modes.push((dest, mask_mode(mode, true, limits.umask), mtime));
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let size = entry.size();
                let file_cap = limits.max_file_bytes.min(limits.max_total_bytes);
                if size > file_cap {
                    return Err(reject(format!(
                        "`{}` is {size} bytes (limit {file_cap}; a sparse file counts at its \
                         full apparent size)",
                        rel.display(),
                    )));
                }
                if stats.bytes + size > limits.max_total_bytes {
                    return Err(reject(format!(
                        "more than {} bytes in total",
                        limits.max_total_bytes
                    )));
                }
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&dest)
                    .map_err(|e| {
                        if e.kind() == io::ErrorKind::AlreadyExists {
                            reject(format!("duplicate entry `{}`", rel.display()))
                        } else {
                            e.into()
                        }
                    })?;
                let copied = copy_sparse(&mut (&mut entry).take(size), &mut f)?;
                if copied != size {
                    return Err(reject(format!("`{}` is truncated", rel.display())));
                }
                f.set_permissions(fs::Permissions::from_mode(mask_mode(
                    mode,
                    false,
                    limits.umask,
                )))?;
                if let Some(t) = mtime {
                    f.set_modified(t)?;
                }
                stats.files += 1;
                stats.bytes += size;
            }
            tar::EntryType::Symlink => {
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| reject(format!("symlink `{}` has no target", rel.display())))?;
                if target.is_empty() {
                    return Err(reject(format!("symlink `{}` has no target", rel.display())));
                }
                let target = os_path(&target)
                    .ok_or_else(|| reject(format!("symlink `{}` target has NUL", rel.display())))?;
                symlink(target, &dest).map_err(|e| {
                    if e.kind() == io::ErrorKind::AlreadyExists {
                        reject(format!("duplicate entry `{}`", rel.display()))
                    } else {
                        e.into()
                    }
                })?;
                stats.symlinks += 1;
            }
            tar::EntryType::Link => {
                return Err(reject(format!("hard link `{}`", rel.display())));
            }
            other => {
                return Err(reject(format!(
                    "unsupported entry type {other:?} for `{}`",
                    rel.display()
                )));
            }
        }
    }

    let tracker = archive.into_inner();
    if tracker.eof {
        return Err(reject(
            "stream ended without the end-of-archive marker (export interrupted?)",
        ));
    }

    // Deepest first, so a parent's mtime is set after its children were touched.
    dir_modes.sort_by_key(|(p, _, _)| std::cmp::Reverse(p.components().count()));
    for (dir, mode, mtime) in dir_modes {
        if let Some(t) = mtime {
            fs::File::open(&dir)?.set_modified(t)?;
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode))?;
    }
    Ok(stats)
}

/// Host mode for a guest-supplied one: permission bits only (no setuid/setgid/sticky), minus
/// `umask`, and always owner-usable (`u+rw` files, `u+rwx` directories).
#[must_use]
pub fn mask_mode(mode: u32, is_dir: bool, umask: u32) -> u32 {
    let m = mode & 0o777 & !umask;
    if is_dir { m | 0o700 } else { m | 0o600 }
}

/// Copy `r` to the start of the fresh file `f`, seeking over all-zero blocks so they become
/// holes; returns the bytes consumed. The final length is set explicitly (a trailing hole).
fn copy_sparse(r: &mut impl Read, f: &mut fs::File) -> io::Result<u64> {
    use std::io::{Seek, SeekFrom, Write};
    const BLOCK: usize = 64 * 1024;
    let mut buf = vec![0u8; BLOCK];
    let mut total = 0u64;
    loop {
        // Fill a whole block (or hit EOF) so hole detection sees aligned blocks.
        let mut n = 0;
        while n < BLOCK {
            match r.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if n == 0 {
            break;
        }
        if buf[..n].iter().all(|&b| b == 0) {
            f.seek(SeekFrom::Current(n as i64))?;
        } else {
            f.write_all(&buf[..n])?;
        }
        total += n as u64;
        if n < BLOCK {
            break;
        }
    }
    f.set_len(total)?;
    Ok(total)
}

/// Bytes → path (any non-NUL byte string is a valid Unix path).
fn os_path(b: &[u8]) -> Option<&Path> {
    (!b.contains(&0)).then(|| Path::new(std::ffi::OsStr::from_bytes(b)))
}

/// Validate an entry path: relative, normal components only, within limits.
fn check_path(raw: &[u8], limits: &UnpackLimits) -> Result<PathBuf> {
    let shown = String::from_utf8_lossy(raw);
    if raw.len() > limits.max_path_len {
        return Err(reject(format!(
            "path longer than {} bytes",
            limits.max_path_len
        )));
    }
    let p = os_path(raw).ok_or_else(|| escape(format!("`{shown}` contains NUL")))?;
    if p.has_root() {
        return Err(escape(format!("`{shown}` is absolute")));
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(escape(format!("`{shown}` contains `..`")));
    }
    let rel = SafeRelPath::new(p).map_err(|_| escape(format!("`{shown}` is not a safe path")))?;
    if rel.is_root() {
        return Err(escape(format!("`{shown}` names the workspace root")));
    }
    let depth = rel.as_path().components().count();
    if depth > limits.max_depth {
        return Err(reject(format!(
            "`{shown}` is deeper than {}",
            limits.max_depth
        )));
    }
    Ok(rel.as_path().to_owned())
}

/// Create missing parents of `rel` under `root`; every existing parent must be a real
/// directory (`lstat`, so a symlink planted by an earlier entry is refused, not followed).
fn ensure_parents(root: &Path, rel: &Path) -> Result<()> {
    let mut cur = root.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for c in &comps[..comps.len().saturating_sub(1)] {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(m) if m.file_type().is_symlink() => {
                return Err(escape(format!(
                    "`{}` would be written through symlink `{}`",
                    rel.display(),
                    cur.strip_prefix(root).unwrap_or(&cur).display()
                )));
            }
            Ok(_) => {
                return Err(reject(format!(
                    "parent of `{}` is not a directory",
                    rel.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Owner-writable until the final mode pass.
                fs::create_dir(&cur)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// Raw ustar header with an arbitrary (unvalidated) name / link name.
    fn raw_entry(
        out: &mut Vec<u8>,
        name: &[u8],
        ty: tar::EntryType,
        mode: u32,
        link: &[u8],
        data: &[u8],
    ) {
        let mut h = tar::Header::new_old();
        h.as_old_mut().name[..name.len()].copy_from_slice(name);
        h.as_old_mut().linkname[..link.len()].copy_from_slice(link);
        h.set_entry_type(ty);
        h.set_mode(mode);
        h.set_size(data.len() as u64);
        h.set_mtime(1_700_000_000);
        h.set_cksum();
        out.extend_from_slice(h.as_bytes());
        out.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
    }

    fn finish(mut out: Vec<u8>) -> Vec<u8> {
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    fn unpack(bytes: &[u8]) -> (tempfile::TempDir, Result<UnpackStats>) {
        let t = tempfile::tempdir().unwrap();
        let staging = t.path().join("staging");
        fs::create_dir(&staging).unwrap();
        let r = unpack_safe(bytes, &staging, &UnpackLimits::default());
        (t, r)
    }

    fn file(name: &str, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        raw_entry(
            &mut v,
            name.as_bytes(),
            tar::EntryType::Regular,
            0o644,
            b"",
            data,
        );
        v
    }

    #[test]
    fn unpacks_basic_tree_with_modes_and_mtime() {
        let mut v = Vec::new();
        raw_entry(&mut v, b"src", tar::EntryType::Directory, 0o755, b"", b"");
        raw_entry(
            &mut v,
            b"src/main.rs",
            tar::EntryType::Regular,
            0o600,
            b"",
            b"fn main(){}",
        );
        raw_entry(
            &mut v,
            b"run.sh",
            tar::EntryType::Regular,
            0o755,
            b"",
            b"#!",
        );
        raw_entry(
            &mut v,
            b"link",
            tar::EntryType::Symlink,
            0o777,
            b"src/main.rs",
            b"",
        );
        raw_entry(&mut v, b"ro", tar::EntryType::Directory, 0o555, b"", b"");
        raw_entry(
            &mut v,
            b"implicit/parent/f",
            tar::EntryType::Regular,
            0o644,
            b"",
            b"x",
        );
        let (t, r) = unpack(&finish(v));
        let stats = r.unwrap();
        let s = t.path().join("staging");
        assert_eq!(stats.files, 3);
        assert_eq!(stats.symlinks, 1);
        assert_eq!(stats.dirs, 2);
        assert_eq!(fs::read(s.join("src/main.rs")).unwrap(), b"fn main(){}");
        let md = fs::metadata(s.join("src/main.rs")).unwrap();
        assert_eq!(md.mode() & 0o7777, 0o600);
        assert_eq!(md.mtime(), 1_700_000_000);
        assert_eq!(
            fs::read_link(s.join("link")).unwrap(),
            Path::new("src/main.rs")
        );
        // 0o555 → owner always keeps u+rwx on directories.
        assert_eq!(fs::metadata(s.join("ro")).unwrap().mode() & 0o7777, 0o755);
        assert_eq!(fs::read(s.join("implicit/parent/f")).unwrap(), b"x");
    }

    fn assert_rejected(bytes: Vec<u8>, needle: &str) {
        let (t, r) = unpack(&bytes);
        let err = r.unwrap_err().to_string();
        assert!(err.contains(needle), "expected `{needle}` in: {err}");
        // Nothing escaped the staging dir.
        let outside: Vec<_> = fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(outside, ["staging"]);
    }

    #[test]
    fn rejects_absolute_paths() {
        assert_rejected(finish(file("/etc/passwd", b"x")), "absolute");
    }

    #[test]
    fn rejects_parent_dir() {
        assert_rejected(finish(file("../evil", b"x")), "`..`");
        assert_rejected(finish(file("a/../../evil", b"x")), "`..`");
    }

    #[test]
    fn rejects_root_entry() {
        let mut v = Vec::new();
        raw_entry(&mut v, b"./", tar::EntryType::Directory, 0o755, b"", b"");
        assert_rejected(finish(v), "workspace root");
    }

    #[test]
    fn rejects_write_through_symlink() {
        let t_out = tempfile::tempdir().unwrap();
        let target = t_out.path().to_str().unwrap().as_bytes().to_vec();
        let mut v = Vec::new();
        raw_entry(&mut v, b"esc", tar::EntryType::Symlink, 0o777, &target, b"");
        raw_entry(
            &mut v,
            b"esc/pwned",
            tar::EntryType::Regular,
            0o644,
            b"",
            b"x",
        );
        assert_rejected(finish(v), "through symlink");
        assert!(fs::read_dir(t_out.path()).unwrap().next().is_none());
    }

    #[test]
    fn rejects_overwriting_symlink_with_file() {
        // Symlink then a file entry at the same path: O_EXCL refuses to follow it.
        let t_out = tempfile::tempdir().unwrap();
        let victim = t_out.path().join("victim");
        fs::write(&victim, b"orig").unwrap();
        let mut v = Vec::new();
        raw_entry(
            &mut v,
            b"f",
            tar::EntryType::Symlink,
            0o777,
            victim.to_str().unwrap().as_bytes(),
            b"",
        );
        raw_entry(&mut v, b"f", tar::EntryType::Regular, 0o644, b"", b"pwned");
        let (_t, r) = unpack(&finish(v));
        assert!(r.unwrap_err().to_string().contains("duplicate"));
        assert_eq!(fs::read(&victim).unwrap(), b"orig");
    }

    #[test]
    fn rejects_hardlinks() {
        let mut v = file("a", b"x");
        raw_entry(&mut v, b"b", tar::EntryType::Link, 0o644, b"a", b"");
        assert_rejected(finish(v), "hard link");
        let mut v = Vec::new();
        raw_entry(
            &mut v,
            b"b",
            tar::EntryType::Link,
            0o644,
            b"/etc/shadow",
            b"",
        );
        assert_rejected(finish(v), "hard link");
    }

    #[test]
    fn rejects_devices_and_fifos() {
        for ty in [
            tar::EntryType::Char,
            tar::EntryType::Block,
            tar::EntryType::Fifo,
        ] {
            let mut v = Vec::new();
            raw_entry(&mut v, b"dev", ty, 0o644, b"", b"");
            assert_rejected(finish(v), "unsupported entry type");
        }
    }

    #[test]
    fn rejects_setuid() {
        let mut v = Vec::new();
        raw_entry(&mut v, b"sh", tar::EntryType::Regular, 0o4755, b"", b"x");
        assert_rejected(finish(v), "setuid");
    }

    #[test]
    fn rejects_duplicates() {
        let mut v = file("a", b"1");
        v.extend(file("a", b"2"));
        assert_rejected(finish(v), "duplicate");
    }

    #[test]
    fn rejects_truncated_stream() {
        // A complete entry but no end-of-archive marker.
        assert_rejected(file("a", b"x"), "end-of-archive");
        // Cut mid-content.
        let mut v = file("a", &[7u8; 2000]);
        v.truncate(700);
        assert_rejected(v, "rejected");
    }

    #[test]
    fn enforces_limits() {
        let t = tempfile::tempdir().unwrap();
        let mut v = file("a", b"12345");
        v.extend(file("b", b"12345"));
        let bytes = finish(v);
        let run = |limits: UnpackLimits| {
            let s = tempfile::tempdir_in(t.path()).unwrap();
            unpack_safe(&bytes[..], s.path(), &limits)
                .unwrap_err()
                .to_string()
        };
        assert!(
            run(UnpackLimits {
                max_entries: 1,
                ..Default::default()
            })
            .contains("entries")
        );
        assert!(
            run(UnpackLimits {
                max_file_bytes: 4,
                ..Default::default()
            })
            .contains("limit 4")
        );
        assert!(
            run(UnpackLimits {
                max_total_bytes: 9,
                ..Default::default()
            })
            .contains("in total")
        );
        assert!(
            run(UnpackLimits {
                max_depth: 0,
                ..Default::default()
            })
            .contains("deeper")
        );
    }

    #[test]
    fn guest_modes_are_masked() {
        let mut v = Vec::new();
        raw_entry(&mut v, b"open", tar::EntryType::Directory, 0o1777, b"", b"");
        raw_entry(
            &mut v,
            b"locked",
            tar::EntryType::Directory,
            0o000,
            b"",
            b"",
        );
        raw_entry(&mut v, b"w", tar::EntryType::Regular, 0o666, b"", b"x");
        raw_entry(&mut v, b"none", tar::EntryType::Regular, 0o000, b"", b"x");
        raw_entry(&mut v, b"x", tar::EntryType::Regular, 0o777, b"", b"x");
        let (t, r) = unpack(&finish(v));
        r.unwrap();
        let s = t.path().join("staging");
        let mode = |n: &str| fs::symlink_metadata(s.join(n)).unwrap().mode() & 0o7777;
        assert_eq!(mode("open"), 0o755, "no world-write, no sticky");
        assert_eq!(mode("locked"), 0o700, "owner can always traverse");
        assert_eq!(mode("w"), 0o644);
        assert_eq!(mode("none"), 0o600);
        assert_eq!(mode("x"), 0o755);
        // A stricter umask.
        assert_eq!(mask_mode(0o775, true, 0o077), 0o700);
        assert_eq!(mask_mode(0o4777, false, 0), 0o777);
    }

    #[test]
    fn capacity_bounds_limits() {
        let l = UnpackLimits::for_capacity(1024 * 1024 * 1024);
        assert_eq!(l.max_total_bytes, 1024 * 1024 * 1024 + CAPACITY_SLACK_BYTES);
        assert_eq!(l.max_file_bytes, l.max_total_bytes, "per-file <= total");
        assert_eq!(l.max_entries, 1_000_000);
        let tiny = UnpackLimits::for_capacity(1024 * 1024);
        assert_eq!(tiny.max_entries, 65_536);
        assert_eq!(tiny.max_file_bytes, 1024 * 1024 + CAPACITY_SLACK_BYTES);
        // Tighter caps (a host quota) survive.
        let quota = UnpackLimits {
            max_total_bytes: 10,
            ..UnpackLimits::default()
        }
        .bounded_by(1 << 40);
        assert_eq!((quota.max_total_bytes, quota.max_file_bytes), (10, 10));
    }

    /// Header claiming `size` bytes, followed by `size` zero bytes streamed lazily (what a
    /// guest exporting `truncate -s <size> f` produces).
    fn sparse_attack(size: u64) -> impl Read {
        let mut h = tar::Header::new_gnu();
        h.set_path("huge").unwrap();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_size(size);
        h.set_cksum();
        let head = h.as_bytes().to_vec();
        io::Cursor::new(head)
            .chain(io::repeat(0).take(size))
            .chain(io::repeat(0).take(1024))
    }

    #[test]
    fn sparse_file_bigger_than_drive_is_rejected_before_writing() {
        let t = tempfile::tempdir().unwrap();
        // 15 GiB apparent size into a 64 MiB drive: refused from the header alone.
        let err = unpack_safe(
            sparse_attack(15 << 30),
            t.path(),
            &UnpackLimits::for_capacity(64 << 20),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("apparent size"), "{err}");
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 0);
    }

    #[test]
    fn zero_runs_are_written_as_holes() {
        let t = tempfile::tempdir().unwrap();
        let size = 32u64 << 20;
        unpack_safe(
            sparse_attack(size),
            t.path(),
            &UnpackLimits::for_capacity(64 << 20),
        )
        .unwrap();
        let md = fs::metadata(t.path().join("huge")).unwrap();
        assert_eq!(md.len(), size);
        assert!(
            md.blocks() * 512 < size / 4,
            "{} blocks allocated for {size} zero bytes",
            md.blocks()
        );
    }

    #[test]
    fn over_quota_total_and_entry_count_are_rejected() {
        let mut v = Vec::new();
        for i in 0..5 {
            v.extend(file(&format!("f{i}"), &[1u8; 1000]));
        }
        let bytes = finish(v);
        let t = tempfile::tempdir().unwrap();
        let quota = UnpackLimits {
            max_total_bytes: 4000,
            ..UnpackLimits::default()
        };
        let s = tempfile::tempdir_in(t.path()).unwrap();
        let err = unpack_safe(&bytes[..], s.path(), &quota).unwrap_err();
        assert!(err.to_string().contains("in total"), "{err}");
        let few = UnpackLimits {
            max_entries: 3,
            ..UnpackLimits::default()
        };
        let s = tempfile::tempdir_in(t.path()).unwrap();
        let err = unpack_safe(&bytes[..], s.path(), &few).unwrap_err();
        assert!(err.to_string().contains("more than 3 entries"), "{err}");
    }

    #[test]
    fn staging_must_be_empty_dir() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("x"), b"").unwrap();
        let empty = finish(Vec::new());
        assert!(unpack_safe(&empty[..], t.path(), &UnpackLimits::default()).is_err());
        assert!(unpack_safe(&empty[..], &t.path().join("x"), &UnpackLimits::default()).is_err());
    }
}
