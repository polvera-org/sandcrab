//! `list_dir` walk.

use std::cmp::Ordering;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use sandcrab_protocol::{DirEntry, EntryKind, ListDirResult, ProtocolError};

use super::{rel_path, require_dir, walker};

pub(super) fn run(
    root: &Path,
    depth: usize,
    respect_gitignore: bool,
    max: usize,
) -> Result<ListDirResult, ProtocolError> {
    require_dir(root, "list_dir")?;
    let mut b = walker(root, respect_gitignore);
    let base = root.components().count();
    b.max_depth(Some(depth)).sort_by_file_path(move |a, b| {
        // walkdir opens and sorts every directory it reaches, including those at
        // `max_depth` whose children it then discards unseen. Their order is irrelevant,
        // so don't stat them: on a virtio-fs share each stat is a host lookup, and the
        // libkrun VMM keeps an fd per looked-up inode (depth 1 over 50 dirs × 100 files:
        // ~3400 → ~1300 host fds; the rest is the kernel's READDIRPLUS on those dirs).
        if beyond(base, depth, a) {
            Ordering::Equal
        } else {
            dirs_first(a, b)
        }
    });

    let mut entries = Vec::new();
    let mut truncated = false;
    for item in b.build() {
        // Unreadable subdirectories / vanished entries are skipped, not fatal.
        let Ok(e) = item else { continue };
        if e.depth() == 0 {
            continue;
        }
        if entries.len() >= max {
            truncated = true;
            break;
        }
        let kind = e.file_type().map_or(EntryKind::Other, |t| {
            if t.is_dir() {
                EntryKind::Dir
            } else if t.is_file() {
                EntryKind::File
            } else if t.is_symlink() {
                EntryKind::Symlink
            } else {
                EntryKind::Other
            }
        });
        let md = e.metadata().ok();
        let size = match kind {
            EntryKind::Dir => 0,
            _ => md.as_ref().map_or(0, fs::Metadata::len),
        };
        let mode = md.map_or(0, |m| m.permissions().mode() & 0o7777);
        entries.push(DirEntry {
            path: rel_path(root, e.path()),
            kind,
            size,
            mode,
        });
    }
    Ok(ListDirResult { entries, truncated })
}

/// Whether `p` (an entry under the root, which has `base` path components) lies deeper than
/// `depth` and so is never yielded.
fn beyond(base: usize, depth: usize, p: &Path) -> bool {
    p.components().count().saturating_sub(base) > depth
}

/// Sibling order: directories (not symlinks to them) first, then by file name.
fn dirs_first(a: &Path, b: &Path) -> Ordering {
    let is_dir = |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    is_dir(b)
        .cmp(&is_dir(a))
        .then_with(|| a.file_name().cmp(&b.file_name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beyond_depth() {
        let root = Path::new("/workspace/data");
        let base = root.components().count();
        assert!(!beyond(base, 1, Path::new("/workspace/data/d000")));
        assert!(beyond(base, 1, Path::new("/workspace/data/d000/f1.bin")));
        assert!(!beyond(base, 2, Path::new("/workspace/data/d000/f1.bin")));
        assert!(beyond(
            base,
            2,
            Path::new("/workspace/data/d000/sub/f1.bin")
        ));
    }

    /// Order is still dirs first at every yielded level, and depth is honoured.
    #[test]
    fn dirs_first_within_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("b/inner")).unwrap();
        fs::create_dir_all(r.join("z")).unwrap();
        fs::write(r.join("a.txt"), "").unwrap();
        fs::write(r.join("b/x.txt"), "").unwrap();
        fs::write(r.join("b/inner/deep.txt"), "").unwrap();
        let names = |d| -> Vec<String> {
            run(r, d, false, 100)
                .unwrap()
                .entries
                .into_iter()
                .map(|e| e.path)
                .collect()
        };
        assert_eq!(names(1), ["b", "z", "a.txt"]);
        assert_eq!(names(2), ["b", "b/inner", "b/x.txt", "z", "a.txt"]);
    }
}
