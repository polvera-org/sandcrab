//! Text file tools: `file_read` / `file_write` / `file_edit` / `file_delete` / `file_move`.
//!
//! Contract (see the `File*Params` / `File*Result` types in `sandcrab-protocol` and
//! `docs/ROADMAP.md`). Paths go through `ctx.resolve_path`; results report absolute paths.
//! Native Rust only — never shell out. Run blocking I/O in `spawn_blocking`.
//! - `file_read`: 1-based `offset` (default 1), `limit` (default
//!   `ctx.limits.file_read_default_lines`). Each line formatted `{n:>6}\t{line}` like `cat -n`;
//!   lines over `ctx.limits.file_read_max_line_chars` are cut and set `truncated`.
//!   Binary detection (NUL byte in the first 8 KiB, or invalid UTF-8) → `is_binary = true`,
//!   empty `content`. Missing file → `NotFound`; directory → `ExecutionFailed` (suggest
//!   `list_dir`); file over `ctx.limits.max_file_bytes` → `TooLarge` unless paging can stream.
//! - `file_write`: create parent dirs; overwrite atomically (temp file in the same dir +
//!   `rename`, preserving the existing mode); `append` appends in place. `created` = did not
//!   exist before. `bytes_written` = bytes of this call.
//! - `file_edit`: exact-match replace. `old_string == new_string` or empty `old_string` →
//!   `InvalidParams`. Zero matches → `NotFound` (hint: re-read the file). >1 match without
//!   `replace_all` → `AmbiguousEdit { path, occurrences }`, file untouched. Write atomically.
//!   `diff` = compact unified diff (`similar` crate, ~3 context lines).
//! - `file_delete`: file / symlink (never follow it) → remove; directory requires
//!   `recursive` else `ExecutionFailed` with a hint. Missing → `NotFound`. Report `kind`.
//! - `file_move`: create destination parents; existing destination without `overwrite` →
//!   `ExecutionFailed`. Try `rename`; on `EXDEV` fall back to copy + delete.
//!
//! Implementation choices (beyond the contract above):
//! - `file_read` **streams** the file line by line, so it has no `max_file_bytes` cap: any
//!   page of an arbitrarily large log can be read. Only the requested lines are kept, and each
//!   kept line buffers at most ~`4 * file_read_max_line_chars` bytes, so a huge single-line
//!   file cannot blow up memory. Lines split on `\n`; a trailing `\r` (CRLF files) is stripped
//!   from the displayed line. A final line without `\n` counts as a line; an empty file has
//!   0 lines. Invalid UTF-8 after the first 8 KiB is shown lossily (U+FFFD). Cut lines end
//!   with `… [line truncated]`. Returned lines are joined with `\n` (no trailing newline).
//!   `offset`/`limit` of 0 → `InvalidParams`.
//! - Symlinks: `file_read` / `file_write` / `file_edit` follow them, and atomic replacement
//!   writes the *final target* (the link itself is preserved, never replaced by a file).
//!   `file_delete` and `file_move` act on the link itself.
//! - Atomic writes: a temp file `.<name>.<random>.crabd-tmp` in the target's directory is
//!   written, `fsync`ed, given the old file's mode (best-effort owner too; new files get
//!   0644) and `rename`d over the target; the directory is then `fsync`ed best-effort. On any
//!   failure the temp file is removed.
//! - `file_edit` on CRLF files: an exact match is tried first. If there is none, the file
//!   contains `\r\n`, and `old_string` uses bare `\n`, both strings are retried with `\n`
//!   converted to `\r\n`, so text copied from `file_read` output edits CRLF files and keeps
//!   their line endings. Files over `max_file_bytes` → `TooLarge`; non-UTF-8 →
//!   `ExecutionFailed`. The diff is capped at 16 KiB.
//! - `file_delete` refuses `/` (`PathNotAllowed`). The sandbox has no jail, so anything else
//!   the daemon uid can remove — including `default_cwd` itself — may be deleted.
//! - `file_move`: `from == to` is a no-op; moving a directory into itself, or onto one of its
//!   own ancestors, → `InvalidParams`. With `overwrite`, a destination directory (or any
//!   destination when the source is a directory) is removed first; file-over-file is a plain
//!   atomic `rename`. The cross-device fallback copies into a temp sibling of the
//!   destination (symlinks copied as links, modes kept), renames it into place, then removes
//!   the source.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use sandcrab_protocol::{
    EntryKind, FileDeleteParams, FileDeleteResult, FileEditParams, FileEditResult, FileMoveParams,
    FileMoveResult, FileReadParams, FileReadResult, FileWriteParams, FileWriteResult,
    ProtocolError,
};

use crate::{ToolContext, context::normalize_lexical};

/// Bytes sniffed at the start of a file for binary detection.
const BINARY_SNIFF_BYTES: u64 = 8 * 1024;
/// Upper bound on the `file_edit` diff returned to the caller.
const MAX_DIFF_BYTES: usize = 16 * 1024;
/// Mode for files created by `file_write` / `file_edit`.
pub(crate) const DEFAULT_NEW_FILE_MODE: u32 = 0o644;
/// Suffix appended to a `file_read` line that was cut.
const LINE_CUT_MARKER: &str = "… [line truncated]";
/// Suffix of temp files used by atomic writes.
pub(crate) const TEMP_SUFFIX: &str = ".crabd-tmp";
/// Max symlink hops followed when resolving a write target.
const MAX_SYMLINK_HOPS: usize = 40;

// ---------------------------------------------------------------------------------------------
// Shared helpers (also used by `transfer`)
// ---------------------------------------------------------------------------------------------

/// Run blocking filesystem work off the async runtime.
pub(crate) async fn blocking<T, F>(f: F) -> Result<T, ProtocolError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ProtocolError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ProtocolError::Internal(format!("blocking task failed: {e}")))?
}

/// Path as a (lossy) string for results and messages.
pub(crate) fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Map an I/O error on `path` to an LLM-readable [`ProtocolError`].
pub(crate) fn io_err(action: &str, path: &Path, e: &io::Error) -> ProtocolError {
    let p = path.display();
    match e.kind() {
        io::ErrorKind::NotFound => {
            ProtocolError::NotFound(format!("{action} {p}: no such file or directory"))
        }
        io::ErrorKind::PermissionDenied => {
            ProtocolError::ExecutionFailed(format!("{action} {p}: permission denied"))
        }
        _ => ProtocolError::ExecutionFailed(format!("{action} {p}: {e}")),
    }
}

/// Error for a path that is a directory where a file was expected.
pub(crate) fn is_dir_err(path: &Path) -> ProtocolError {
    ProtocolError::ExecutionFailed(format!(
        "{} is a directory, not a file; use `list_dir` to see its entries",
        path.display()
    ))
}

/// Follow symlinks at `path` (lexically, so dangling links resolve to where the target
/// *would* be) and return the final non-link path. Non-links are returned unchanged.
pub(crate) fn resolve_write_target(path: &Path) -> Result<PathBuf, ProtocolError> {
    let mut cur = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                let link = fs::read_link(&cur).map_err(|e| io_err("cannot read link", &cur, &e))?;
                let base = cur.parent().unwrap_or(Path::new("/"));
                cur = normalize_lexical(&base.join(link));
            }
            _ => return Ok(cur),
        }
    }
    Err(ProtocolError::ExecutionFailed(format!(
        "{}: too many levels of symbolic links",
        path.display()
    )))
}

/// Create the parent directories of `path`.
pub(crate) fn create_parents(path: &Path) -> Result<(), ProtocolError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| io_err("cannot create parent directories of", path, &e))?;
        }
    }
    Ok(())
}

/// Atomically replace (or create) `target` with `bytes`: temp file in the same directory,
/// `fsync`, mode/owner, `rename`, best-effort directory `fsync`. The temp file never
/// survives a failure. `mode` overrides the mode; otherwise the existing file's mode is kept
/// (new files get [`DEFAULT_NEW_FILE_MODE`]). `target` must not be a symlink (resolve first).
pub(crate) fn write_atomic(target: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let name: String = target
        .file_name()
        .map(|n| n.to_string_lossy().chars().take(64).collect())
        .unwrap_or_default();
    let existing = fs::metadata(target).ok();
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(TEMP_SUFFIX)
        .tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    if let Some(m) = &existing {
        // Best-effort: only root (or the owner, for its own groups) may chown.
        let _ = std::os::unix::fs::fchown(tmp.as_file(), Some(m.uid()), Some(m.gid()));
    }
    let mode = mode
        .or_else(|| existing.as_ref().map(|m| m.permissions().mode() & 0o7777))
        .unwrap_or(DEFAULT_NEW_FILE_MODE);
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    tmp.as_file().sync_all()?;
    tmp.persist(target).map_err(|e| e.error)?;
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// file_read
// ---------------------------------------------------------------------------------------------

/// Read a text file with line numbers.
pub async fn file_read(
    ctx: &ToolContext,
    p: FileReadParams,
) -> Result<FileReadResult, ProtocolError> {
    let offset = p.offset.unwrap_or(1);
    let limit = p.limit.unwrap_or(ctx.limits.file_read_default_lines);
    if offset == 0 {
        return Err(ProtocolError::InvalidParams(
            "`offset` is 1-based; use 1 for the first line".into(),
        ));
    }
    if limit == 0 {
        return Err(ProtocolError::InvalidParams(
            "`limit` must be at least 1".into(),
        ));
    }
    let path = ctx.resolve_path(&p.path);
    let max_chars = ctx.limits.file_read_max_line_chars;
    blocking(move || read_lines(&path, offset, limit, max_chars)).await
}

/// True if `head` (the first bytes of a file) looks binary.
pub(crate) fn looks_binary(head: &[u8]) -> bool {
    if head.contains(&0) {
        return true;
    }
    match std::str::from_utf8(head) {
        Ok(_) => false,
        // `error_len() == None`: a multi-byte char cut by the sniff window — still text.
        Err(e) => e.error_len().is_some(),
    }
}

fn read_lines(
    path: &Path,
    offset: u64,
    limit: u64,
    max_chars: usize,
) -> Result<FileReadResult, ProtocolError> {
    let meta = fs::metadata(path).map_err(|e| io_err("cannot read", path, &e))?;
    if meta.is_dir() {
        return Err(is_dir_err(path));
    }
    if !meta.is_file() {
        return Err(ProtocolError::ExecutionFailed(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut file = File::open(path).map_err(|e| io_err("cannot open", path, &e))?;

    let mut head = Vec::with_capacity(BINARY_SNIFF_BYTES as usize);
    (&mut file)
        .take(BINARY_SNIFF_BYTES)
        .read_to_end(&mut head)
        .map_err(|e| io_err("cannot read", path, &e))?;
    if looks_binary(&head) {
        return Ok(FileReadResult {
            content: String::new(),
            total_lines: 0,
            start_line: offset,
            end_line: offset - 1,
            truncated: false,
            is_binary: true,
        });
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|e| io_err("cannot read", path, &e))?;

    let end = offset.saturating_add(limit); // exclusive
    let keep_cap = max_chars.saturating_mul(4).saturating_add(4);
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut out: Vec<String> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut lines_done: u64 = 0;
    let mut in_line = false;
    let mut cut_any = false;
    let mut last_emitted: Option<u64> = None;

    let mut emit = |n: u64, raw: &[u8], out: &mut Vec<String>| {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        let text = String::from_utf8_lossy(raw);
        let line = if text.chars().count() > max_chars {
            cut_any = true;
            let mut s: String = text.chars().take(max_chars).collect();
            s.push_str(LINE_CUT_MARKER);
            s
        } else {
            text.into_owned()
        };
        out.push(format!("{n:>6}\t{line}"));
        last_emitted = Some(n);
    };

    loop {
        let chunk = reader
            .fill_buf()
            .map_err(|e| io_err("cannot read", path, &e))?;
        if chunk.is_empty() {
            break;
        }
        let (consumed, ends_line) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        let part_len = if ends_line { consumed - 1 } else { consumed };
        let this = lines_done + 1;
        let wanted = this >= offset && this < end;
        if wanted {
            let room = keep_cap.saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..part_len.min(room)]);
        }
        in_line = true;
        reader.consume(consumed);
        if ends_line {
            lines_done += 1;
            in_line = false;
            if wanted {
                emit(this, &buf, &mut out);
                buf.clear();
            }
        }
    }
    if in_line {
        lines_done += 1;
        let this = lines_done;
        if this >= offset && this < end {
            emit(this, &buf, &mut out);
        }
    }

    let end_line = last_emitted.unwrap_or(offset - 1);
    Ok(FileReadResult {
        content: out.join("\n"),
        total_lines: lines_done,
        start_line: offset,
        end_line,
        truncated: cut_any || end_line < lines_done,
        is_binary: false,
    })
}

// ---------------------------------------------------------------------------------------------
// file_write
// ---------------------------------------------------------------------------------------------

/// Create, overwrite or append to a text file.
pub async fn file_write(
    ctx: &ToolContext,
    p: FileWriteParams,
) -> Result<FileWriteResult, ProtocolError> {
    let path = ctx.resolve_path(&p.path);
    blocking(move || {
        let target = resolve_write_target(&path)?;
        let existed = match fs::metadata(&target) {
            Ok(m) if m.is_dir() => return Err(is_dir_err(&target)),
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(io_err("cannot write", &target, &e)),
        };
        create_parents(&target)?;
        let bytes = p.content.as_bytes();
        if p.append {
            let mut f = OpenOptions::new()
                .append(true)
                .create(true)
                .mode(DEFAULT_NEW_FILE_MODE)
                .open(&target)
                .map_err(|e| io_err("cannot open", &target, &e))?;
            f.write_all(bytes)
                .and_then(|()| f.sync_data())
                .map_err(|e| io_err("cannot append to", &target, &e))?;
        } else {
            write_atomic(&target, bytes, None).map_err(|e| io_err("cannot write", &target, &e))?;
        }
        Ok(FileWriteResult {
            path: path_str(&path),
            bytes_written: bytes.len() as u64,
            created: !existed,
        })
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// file_edit
// ---------------------------------------------------------------------------------------------

/// Exact string replacement in a file.
pub async fn file_edit(
    ctx: &ToolContext,
    p: FileEditParams,
) -> Result<FileEditResult, ProtocolError> {
    if p.old_string.is_empty() {
        return Err(ProtocolError::InvalidParams(
            "`old_string` must not be empty; use `file_write` to create or replace a whole file"
                .into(),
        ));
    }
    if p.old_string == p.new_string {
        return Err(ProtocolError::InvalidParams(
            "`old_string` and `new_string` are identical; nothing to change".into(),
        ));
    }
    let path = ctx.resolve_path(&p.path);
    let max_bytes = ctx.limits.max_file_bytes;
    blocking(move || edit_file(&path, &p, max_bytes)).await
}

fn edit_file(
    path: &Path,
    p: &FileEditParams,
    max_bytes: u64,
) -> Result<FileEditResult, ProtocolError> {
    let target = resolve_write_target(path)?;
    let meta = fs::metadata(&target).map_err(|e| io_err("cannot edit", path, &e))?;
    if meta.is_dir() {
        return Err(is_dir_err(path));
    }
    if meta.len() > max_bytes {
        return Err(ProtocolError::TooLarge {
            size: meta.len(),
            limit: max_bytes,
        });
    }
    let bytes = fs::read(&target).map_err(|e| io_err("cannot read", path, &e))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        ProtocolError::ExecutionFailed(format!(
            "{} is not valid UTF-8 text (binary?); `file_edit` only edits text files — use `upload` to replace binary files",
            path.display()
        ))
    })?;

    let (old, new, occurrences) = find_edit(&text, &p.old_string, &p.new_string);
    if occurrences == 0 {
        return Err(ProtocolError::NotFound(not_found_hint(
            path,
            &text,
            &p.old_string,
        )));
    }
    if occurrences > 1 && !p.replace_all {
        return Err(ProtocolError::AmbiguousEdit {
            path: path_str(path),
            occurrences: occurrences as u64,
        });
    }
    let updated = if p.replace_all {
        text.replace(&old, &new)
    } else {
        text.replacen(&old, &new, 1)
    };
    write_atomic(&target, updated.as_bytes(), None)
        .map_err(|e| io_err("cannot write", path, &e))?;
    Ok(FileEditResult {
        replacements: occurrences as u64,
        diff: unified_diff(&path_str(path), &text, &updated),
    })
}

/// Pick the strings to replace (exact first, then the CRLF-adapted pair) and count matches.
fn find_edit(text: &str, old: &str, new: &str) -> (String, String, usize) {
    let exact = text.matches(old).count();
    if exact == 0 && old.contains('\n') && !old.contains("\r\n") && text.contains("\r\n") {
        let old_crlf = old.replace('\n', "\r\n");
        let count = text.matches(old_crlf.as_str()).count();
        if count > 0 {
            let new_crlf = new.replace("\r\n", "\n").replace('\n', "\r\n");
            return (old_crlf, new_crlf, count);
        }
    }
    (old.to_owned(), new.to_owned(), exact)
}

/// Message for an `old_string` with no match, pointing at lines that contain its first line.
fn not_found_hint(path: &Path, text: &str, old: &str) -> String {
    let mut msg = format!("`old_string` was not found in {}.", path.display());
    let first = old.lines().map(str::trim).find(|l| !l.is_empty());
    if let Some(first) = first {
        let hits: Vec<String> = text
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains(first))
            .take(5)
            .map(|(i, _)| (i + 1).to_string())
            .collect();
        if !hits.is_empty() {
            msg.push_str(&format!(
                " Its first line appears at line(s) {} but the surrounding text differs (check whitespace, indentation and the following lines).",
                hits.join(", ")
            ));
        }
    }
    msg.push_str(" Re-read the file with `file_read` and copy the exact text (without the line-number prefix).");
    msg
}

/// Compact unified diff (3 lines of context), capped at [`MAX_DIFF_BYTES`].
fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let diff = similar::TextDiff::configure()
        .timeout(Duration::from_secs(2))
        .diff_lines(old, new);
    let mut s = diff
        .unified_diff()
        .context_radius(3)
        .header(path, path)
        .to_string();
    if s.len() > MAX_DIFF_BYTES {
        let mut cut = MAX_DIFF_BYTES;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        let more = s.len() - cut;
        s.truncate(cut);
        s.push_str(&format!("\n… [diff truncated: {more} more bytes]\n"));
    }
    s
}

// ---------------------------------------------------------------------------------------------
// file_delete
// ---------------------------------------------------------------------------------------------

/// Delete a file, symlink or directory.
pub async fn file_delete(
    ctx: &ToolContext,
    p: FileDeleteParams,
) -> Result<FileDeleteResult, ProtocolError> {
    let path = ctx.resolve_path(&p.path);
    blocking(move || {
        if path == Path::new("/") {
            return Err(ProtocolError::PathNotAllowed(
                "refusing to delete `/`".into(),
            ));
        }
        let meta = fs::symlink_metadata(&path).map_err(|e| io_err("cannot delete", &path, &e))?;
        let ft = meta.file_type();
        let kind = if ft.is_symlink() {
            EntryKind::Symlink
        } else if ft.is_dir() {
            EntryKind::Dir
        } else if ft.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        if kind == EntryKind::Dir {
            if !p.recursive {
                return Err(ProtocolError::ExecutionFailed(format!(
                    "{} is a directory; set `recursive: true` to delete it and everything inside",
                    path.display()
                )));
            }
            // `remove_dir_all` never follows symlinks inside the tree.
            fs::remove_dir_all(&path).map_err(|e| io_err("cannot delete", &path, &e))?;
        } else {
            // Symlinks are removed themselves, never their target.
            fs::remove_file(&path).map_err(|e| io_err("cannot delete", &path, &e))?;
        }
        Ok(FileDeleteResult {
            path: path_str(&path),
            kind,
        })
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// file_move
// ---------------------------------------------------------------------------------------------

/// Move or rename a file or directory.
pub async fn file_move(
    ctx: &ToolContext,
    p: FileMoveParams,
) -> Result<FileMoveResult, ProtocolError> {
    let from = ctx.resolve_path(&p.from);
    let to = ctx.resolve_path(&p.to);
    let overwrite = p.overwrite;
    blocking(move || {
        move_entry(&from, &to, overwrite)?;
        Ok(FileMoveResult {
            from: path_str(&from),
            to: path_str(&to),
        })
    })
    .await
}

fn move_entry(from: &Path, to: &Path, overwrite: bool) -> Result<(), ProtocolError> {
    let src = fs::symlink_metadata(from).map_err(|e| io_err("cannot move", from, &e))?;
    if from == to {
        return Ok(());
    }
    if from == Path::new("/") {
        return Err(ProtocolError::PathNotAllowed("refusing to move `/`".into()));
    }
    let src_is_dir = src.file_type().is_dir();
    if src_is_dir && to.starts_with(from) {
        return Err(ProtocolError::InvalidParams(format!(
            "cannot move directory {} into itself ({})",
            from.display(),
            to.display()
        )));
    }
    if from.starts_with(to) {
        return Err(ProtocolError::InvalidParams(format!(
            "cannot move {} onto its own ancestor {}",
            from.display(),
            to.display()
        )));
    }
    match fs::symlink_metadata(to) {
        Ok(dst) => {
            if !overwrite {
                return Err(ProtocolError::ExecutionFailed(format!(
                    "destination {} already exists; set `overwrite: true` to replace it",
                    to.display()
                )));
            }
            let dst_is_dir = dst.file_type().is_dir();
            if dst_is_dir {
                fs::remove_dir_all(to).map_err(|e| io_err("cannot replace", to, &e))?;
            } else if src_is_dir {
                fs::remove_file(to).map_err(|e| io_err("cannot replace", to, &e))?;
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err("cannot move to", to, &e)),
    }
    create_parents(to)?;
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => copy_then_remove(from, to)
            .map_err(|e| io_err("cannot move (cross-device copy)", from, &e)),
        Err(e) => Err(io_err("cannot move", from, &e)),
    }
}

fn is_cross_device(e: &io::Error) -> bool {
    // EXDEV is 18 on Linux and macOS.
    e.kind() == io::ErrorKind::CrossesDevices || e.raw_os_error() == Some(18)
}

/// Cross-device move: copy `from` into a temp sibling of `to`, rename it into place, then
/// remove `from`. The temp copy is removed on failure; `from` is only removed once `to`
/// exists. `to` must not be an existing directory.
pub(crate) fn copy_then_remove(from: &Path, to: &Path) -> io::Result<()> {
    let parent = match to.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let name: String = to
        .file_name()
        .map(|n| n.to_string_lossy().chars().take(64).collect())
        .unwrap_or_default();
    // Reserve a unique temp name with `tempfile`, then reuse the path for the copy.
    let tmp_dir = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".crabd-move")
        .tempdir_in(parent)?;
    let tmp = tmp_dir.path().join("entry");
    let res = copy_tree(from, &tmp).and_then(|()| fs::rename(&tmp, to));
    // `tmp_dir` drop removes the staging dir (and a partial copy on failure).
    drop(tmp_dir);
    res?;
    let src = fs::symlink_metadata(from)?;
    if src.file_type().is_dir() {
        fs::remove_dir_all(from)
    } else {
        fs::remove_file(from)
    }
}

/// Recursive copy that preserves symlinks (as links) and permission bits.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    } else if ft.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        fs::set_permissions(
            to,
            fs::Permissions::from_mode(meta.permissions().mode() & 0o7777),
        )
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_sniff() {
        assert!(!looks_binary(b"hello\nworld"));
        assert!(!looks_binary("héllo ✓".as_bytes()));
        assert!(looks_binary(b"a\0b"));
        assert!(looks_binary(&[0xff, 0xfe, b'a']));
        // Multi-byte char cut by the sniff window is still text.
        let s = "ab✓".as_bytes();
        assert!(!looks_binary(&s[..s.len() - 1]));
    }

    #[test]
    fn crlf_adaptation() {
        let text = "a\r\nb\r\nc\r\n";
        let (old, new, n) = find_edit(text, "a\nb", "x\ny");
        assert_eq!((old.as_str(), new.as_str(), n), ("a\r\nb", "x\r\ny", 1));
        let (_, _, n) = find_edit("a\nb\n", "a\nb", "z");
        assert_eq!(n, 1);
    }

    #[test]
    fn diff_is_capped() {
        let old: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..5000).map(|i| format!("LINE {i}\n")).collect();
        let d = unified_diff("f", &old, &new);
        assert!(d.len() < MAX_DIFF_BYTES + 100);
        assert!(d.contains("diff truncated"));
    }

    #[test]
    fn copy_then_remove_file_and_tree() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        fs::write(&f, "data").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o750)).unwrap();
        let g = dir.path().join("out/g.txt");
        fs::create_dir_all(g.parent().unwrap()).unwrap();
        copy_then_remove(&f, &g).unwrap();
        assert!(!f.exists());
        assert_eq!(fs::read_to_string(&g).unwrap(), "data");
        assert_eq!(
            fs::metadata(&g).unwrap().permissions().mode() & 0o777,
            0o750
        );

        let src = dir.path().join("tree");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("sub/a"), "A").unwrap();
        std::os::unix::fs::symlink("sub/a", src.join("link")).unwrap();
        let dst = dir.path().join("moved");
        copy_then_remove(&src, &dst).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(dst.join("sub/a")).unwrap(), "A");
        assert!(
            fs::symlink_metadata(dst.join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(dst.join("link")).unwrap(), Path::new("sub/a"));
        // No staging dirs left behind.
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("crabd"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn copy_then_remove_failure_keeps_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::write(&src, "x").unwrap();
        // Destination parent missing → the staging dir cannot be created; source kept.
        let err = copy_then_remove(&src, &dir.path().join("missing/dst"));
        assert!(err.is_err());
        assert!(src.exists());
    }
}
