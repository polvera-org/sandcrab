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
    b.max_depth(Some(depth)).sort_by_file_path(dirs_first);

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

/// Sibling order: directories (not symlinks to them) first, then by file name.
fn dirs_first(a: &Path, b: &Path) -> Ordering {
    let is_dir = |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    is_dir(b)
        .cmp(&is_dir(a))
        .then_with(|| a.file_name().cmp(&b.file_name()))
}
