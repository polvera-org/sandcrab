//! Guest side of workspace sync-back: pack a tree into the `sandcrab-tar-v1` archive format
//! (see [`sandcrab_protocol::init::archive`]) and stream it over an async connection.
//!
//! Before acking an export, the control handler runs [`precheck`]: a stat-only walk that
//! refuses (with a clear JSON-RPC error, while one can still be sent) a tree whose archive
//! would exceed the drive's capacity — sparse files and hard links are stored at their full
//! apparent size, so a 15 GiB `truncate -s` on a 1 GiB drive is refused here, naming the
//! file. The host enforces the same bound independently (`UnpackLimits::for_capacity`).

use std::{
    fs::{self, Metadata},
    io::{self, BufWriter, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use sandcrab_protocol::init::archive::{DIR_PERMISSION_MASK, PERMISSION_MASK, SKIPPED_ROOT_ENTRY};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};
use tracing::debug;

/// What [`pack_tree`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackStats {
    pub dirs: u64,
    pub files: u64,
    pub symlinks: u64,
    /// File content bytes (excluding headers / padding).
    pub bytes: u64,
    /// Sockets, FIFOs and device nodes that were skipped.
    pub skipped_special: u64,
    /// Entries on another filesystem (nested mounts) that were skipped.
    pub skipped_other_fs: u64,
}

/// Pack the tree under `root` into `w` (see the format rules in
/// [`sandcrab_protocol::init::archive`]). `owner` overrides the recorded uid/gid.
///
/// Entries that vanish while packing are skipped; a file that shrinks is zero-padded and one
/// that grows is truncated to its size at `stat` time, so the archive always stays well-formed.
pub fn pack_tree<W: Write>(root: &Path, owner: Option<(u32, u32)>, w: W) -> io::Result<PackStats> {
    let root_md = fs::symlink_metadata(root)?;
    if !root_md.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", root.display()),
        ));
    }
    let root_dev = root_md.dev();
    let mut stats = PackStats::default();
    let mut builder = tar::Builder::new(w);
    // Stack of (relative path, absolute path); children pushed in reverse byte order so they
    // pop in order, giving a deterministic pre-order walk.
    let mut stack: Vec<(PathBuf, PathBuf)> = Vec::new();
    push_children(root, Path::new(""), true, &mut stack)?;

    while let Some((rel, abs)) = stack.pop() {
        let md = match fs::symlink_metadata(&abs) {
            Ok(md) => md,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let ft = md.file_type();
        if md.dev() != root_dev {
            stats.skipped_other_fs += 1;
            continue;
        }
        let mut header = base_header(&md, owner);
        if ft.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(md.mode() & DIR_PERMISSION_MASK);
            header.set_size(0);
            builder.append_data(&mut header, &rel, io::empty())?;
            stats.dirs += 1;
            match push_children(&abs, &rel, false, &mut stack) {
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
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(PERMISSION_MASK);
            header.set_size(0);
            builder.append_link(&mut header, &rel, &target)?;
            stats.symlinks += 1;
        } else if ft.is_file() {
            // O_NOFOLLOW: never read through a symlink swapped in after the lstat.
            let file = match fs::OpenOptions::new()
                .read(true)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                .open(&abs)
            {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    debug!(path = %abs.display(), error = %e, "cannot open file for export");
                    return Err(e);
                }
            };
            let size = md.len();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(md.mode() & PERMISSION_MASK);
            header.set_size(size);
            builder.append_data(&mut header, &rel, ExactSize::new(file, size))?;
            stats.files += 1;
            stats.bytes += size;
        } else {
            stats.skipped_special += 1;
        }
    }
    let mut w = builder.into_inner()?;
    w.flush()?;
    Ok(stats)
}

/// What a [`precheck`] walk saw (same filters as [`pack_tree`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeScan {
    /// Directories + files + symlinks.
    pub entries: u64,
    /// Sum of regular-file apparent sizes (what the archive will carry).
    pub apparent_bytes: u64,
    /// Sum of regular-file allocated sizes.
    pub allocated_bytes: u64,
    /// The file whose apparent size exceeds its allocation the most:
    /// (relative path, apparent, allocated).
    pub most_sparse: Option<(PathBuf, u64, u64)>,
}

/// Stat-only walk of `root` (no file contents read).
pub fn scan_tree(root: &Path) -> io::Result<TreeScan> {
    let root_dev = fs::symlink_metadata(root)?.dev();
    let mut scan = TreeScan::default();
    let mut stack: Vec<(PathBuf, PathBuf)> = Vec::new();
    push_children(root, Path::new(""), true, &mut stack)?;
    while let Some((rel, abs)) = stack.pop() {
        let md = match fs::symlink_metadata(&abs) {
            Ok(md) => md,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let ft = md.file_type();
        if md.dev() != root_dev || !(ft.is_dir() || ft.is_file() || ft.is_symlink()) {
            continue;
        }
        scan.entries += 1;
        if ft.is_dir() {
            match push_children(&abs, &rel, false, &mut stack) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        } else if ft.is_file() {
            let (size, alloc) = (md.len(), md.blocks().saturating_mul(512));
            scan.apparent_bytes = scan.apparent_bytes.saturating_add(size);
            scan.allocated_bytes = scan.allocated_bytes.saturating_add(alloc);
            let gap = size.saturating_sub(alloc);
            if gap > 0
                && scan
                    .most_sparse
                    .as_ref()
                    .is_none_or(|(_, s, a)| gap > s.saturating_sub(*a))
            {
                scan.most_sparse = Some((rel, size, alloc));
            }
        }
    }
    Ok(scan)
}

/// Refuse an export whose archive content would exceed `capacity` bytes.
pub fn check_fits(scan: &TreeScan, capacity: u64) -> io::Result<()> {
    if scan.apparent_bytes <= capacity {
        return Ok(());
    }
    let hint = match &scan.most_sparse {
        Some((p, size, alloc)) => format!(
            "; sparse file `{}` alone is {size} bytes apparent / {alloc} allocated",
            p.display()
        ),
        None => String::new(),
    };
    Err(io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!(
            "workspace export would carry {} bytes of file content but the drive holds {capacity} \
             (sparse files and hard links are exported at their full apparent size){hint}; \
             shrink or delete them before the sandbox is destroyed",
            scan.apparent_bytes
        ),
    ))
}

/// Size of the filesystem holding `root` (`statvfs`: blocks × fragment size).
pub fn fs_capacity(root: &Path) -> io::Result<u64> {
    let st = rustix::fs::statvfs(root).map_err(io::Error::from)?;
    Ok(st.f_blocks.saturating_mul(st.f_frsize))
}

/// [`scan_tree`] + [`check_fits`] against the capacity of `root`'s filesystem.
pub fn precheck(root: &Path) -> io::Result<TreeScan> {
    let scan = scan_tree(root)?;
    check_fits(&scan, fs_capacity(root)?)?;
    Ok(scan)
}

fn push_children(
    dir: &Path,
    rel: &Path,
    is_root: bool,
    stack: &mut Vec<(PathBuf, PathBuf)>,
) -> io::Result<()> {
    let mut names: Vec<_> = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .filter(|n| !(is_root && n == SKIPPED_ROOT_ENTRY))
        .collect();
    names.sort_by(|a, b| a.as_encoded_bytes().cmp(b.as_encoded_bytes()));
    for name in names.into_iter().rev() {
        stack.push((rel.join(&name), dir.join(&name)));
    }
    Ok(())
}

fn base_header(md: &Metadata, owner: Option<(u32, u32)>) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    let (uid, gid) = owner.unwrap_or((md.uid(), md.gid()));
    h.set_uid(u64::from(uid));
    h.set_gid(u64::from(gid));
    h.set_mtime(u64::try_from(md.mtime()).unwrap_or(0));
    h
}

/// Reader that yields exactly `size` bytes: truncates growth, zero-pads shrinkage.
struct ExactSize<R> {
    inner: io::Take<R>,
    remaining: u64,
}

impl<R: Read> ExactSize<R> {
    fn new(inner: R, size: u64) -> Self {
        Self {
            inner: inner.take(size),
            remaining: size,
        }
    }
}

impl<R: Read> Read for ExactSize<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let n = match self.inner.read(buf)? {
            0 => {
                let n = usize::try_from(self.remaining)
                    .unwrap_or(usize::MAX)
                    .min(buf.len());
                buf[..n].fill(0);
                n
            }
            n => n,
        };
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// `Write` adapter sending chunks to an async consumer (backpressure via a bounded channel).
struct ChunkSender(mpsc::Sender<Vec<u8>>);

impl Write for ChunkSender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .blocking_send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "export consumer gone"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

const CHUNK: usize = 256 * 1024;

/// Pack `root` on a blocking thread and stream the archive into `w`. Does not shut `w` down.
pub async fn stream_tree<W: AsyncWrite + Unpin>(
    root: PathBuf,
    owner: Option<(u32, u32)>,
    w: &mut W,
) -> io::Result<PackStats> {
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
    let packer = tokio::task::spawn_blocking(move || {
        pack_tree(
            &root,
            owner,
            BufWriter::with_capacity(CHUNK, ChunkSender(tx)),
        )
    });
    let mut write_err = None;
    while let Some(chunk) = rx.recv().await {
        if let Err(e) = w.write_all(&chunk).await {
            write_err = Some(e);
            break;
        }
    }
    drop(rx);
    let packed = packer
        .await
        .map_err(|e| io::Error::other(format!("pack task failed: {e}")))?;
    if let Some(e) = write_err {
        return Err(e);
    }
    let stats = packed?;
    w.flush().await?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn tree() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        fs::create_dir_all(r.join("src/nested")).unwrap();
        fs::create_dir_all(r.join("lost+found")).unwrap();
        fs::write(r.join("b.txt"), b"bee").unwrap();
        fs::write(r.join("a.txt"), b"ay").unwrap();
        fs::write(r.join("src/nested/deep.rs"), b"fn main() {}").unwrap();
        fs::write(r.join("run.sh"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(r.join("run.sh"), fs::Permissions::from_mode(0o4755)).unwrap();
        symlink("../a.txt", r.join("src/link")).unwrap();
        symlink("/etc/passwd", r.join("abs-link")).unwrap();
        fs::hard_link(r.join("a.txt"), r.join("src/hard")).unwrap();
        let long = "n".repeat(150);
        fs::write(r.join(&long), b"long").unwrap();
        // A FIFO via std is not possible without unsafe/libc; a socket is a special file too.
        let _sock = std::os::unix::net::UnixListener::bind(r.join("sock")).unwrap();
        t
    }

    fn entries(archive: &[u8]) -> Vec<(String, tar::EntryType, u32, Vec<u8>)> {
        let mut a = tar::Archive::new(archive);
        a.entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let path = e.path().unwrap().to_string_lossy().into_owned();
                let ty = e.header().entry_type();
                let mode = e.header().mode().unwrap();
                let mut data = Vec::new();
                if ty == tar::EntryType::Symlink {
                    data = e
                        .link_name()
                        .unwrap()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                        .into_bytes();
                } else {
                    e.read_to_end(&mut data).unwrap();
                }
                (path, ty, mode, data)
            })
            .collect()
    }

    #[test]
    fn packs_deterministically_in_byte_order() {
        let t = tree();
        let mut a = Vec::new();
        let stats = pack_tree(t.path(), Some((1000, 1000)), &mut a).unwrap();
        let mut b = Vec::new();
        pack_tree(t.path(), Some((1000, 1000)), &mut b).unwrap();
        assert_eq!(a, b, "same tree, same bytes");

        let es = entries(&a);
        let names: Vec<_> = es.iter().map(|e| e.0.as_str()).collect();
        let long = "n".repeat(150);
        assert_eq!(
            names,
            [
                "a.txt",
                "abs-link",
                "b.txt",
                long.as_str(),
                "run.sh",
                "src",
                "src/hard",
                "src/link",
                "src/nested",
                "src/nested/deep.rs",
            ]
        );
        assert_eq!(stats.files, 6);
        assert_eq!(stats.dirs, 2);
        assert_eq!(stats.symlinks, 2);
        assert_eq!(stats.skipped_special, 1, "socket skipped");

        let get = |n: &str| es.iter().find(|e| e.0 == n).unwrap();
        assert_eq!(get("run.sh").2, 0o755, "setuid dropped");
        assert_eq!(
            get("src/hard").1,
            tar::EntryType::Regular,
            "hardlink as file"
        );
        assert_eq!(get("src/hard").3, b"ay");
        assert_eq!(get("src/link").3, b"../a.txt");
        assert_eq!(get("abs-link").3, b"/etc/passwd");
        assert_eq!(get("src").1, tar::EntryType::Directory);
    }

    #[test]
    fn owner_override_and_mode() {
        let t = tree();
        let mut a = Vec::new();
        pack_tree(t.path(), Some((4242, 4343)), &mut a).unwrap();
        let mut ar = tar::Archive::new(&a[..]);
        for e in ar.entries().unwrap() {
            let e = e.unwrap();
            assert_eq!(e.header().uid().unwrap(), 4242);
            assert_eq!(e.header().gid().unwrap(), 4343);
        }
    }

    #[test]
    fn exact_size_pads_and_truncates() {
        let mut out = Vec::new();
        ExactSize::new(&b"abc"[..], 5)
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, b"abc\0\0");
        out.clear();
        ExactSize::new(&b"abcdef"[..], 2)
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, b"ab");
    }

    #[test]
    fn rejects_non_directory_root() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("f"), b"").unwrap();
        assert!(pack_tree(&t.path().join("f"), None, io::sink()).is_err());
    }

    #[test]
    fn precheck_refuses_sparse_file_bigger_than_drive() {
        let t = tree();
        let f = fs::File::create(t.path().join("huge")).unwrap();
        f.set_len(15 << 30).unwrap(); // `truncate -s 15G huge`
        let scan = scan_tree(t.path()).unwrap();
        assert!(scan.apparent_bytes >= 15 << 30);
        let (p, size, alloc) = scan.most_sparse.clone().unwrap();
        assert_eq!((p.as_path(), size), (Path::new("huge"), 15 << 30));
        assert!(alloc < size);
        assert_eq!(scan.entries, 11, "socket not counted");
        let err = check_fits(&scan, 1 << 30).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        assert!(err.to_string().contains("`huge`"), "{err}");
        check_fits(&scan, 16 << 30).unwrap();
        assert!(fs_capacity(t.path()).unwrap() > 0);
    }

    #[test]
    fn precheck_counts_hard_links_per_name() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), vec![1u8; 4096]).unwrap();
        for i in 0..9 {
            fs::hard_link(t.path().join("a"), t.path().join(format!("h{i}"))).unwrap();
        }
        let scan = scan_tree(t.path()).unwrap();
        assert_eq!(scan.apparent_bytes, 10 * 4096);
        assert!(check_fits(&scan, 5 * 4096).is_err());
    }

    #[tokio::test]
    async fn stream_matches_sync_pack() {
        let t = tree();
        let mut sync = Vec::new();
        pack_tree(t.path(), None, &mut sync).unwrap();
        let mut streamed = Vec::new();
        let stats = stream_tree(t.path().to_owned(), None, &mut streamed)
            .await
            .unwrap();
        assert_eq!(sync, streamed);
        assert_eq!(stats.files, 6);
    }
}
