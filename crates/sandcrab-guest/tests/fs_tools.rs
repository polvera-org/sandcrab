//! Tests for the native file tools: `file_read`, `file_write`, `file_edit`, `file_delete`,
//! `file_move`.

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
};

use sandcrab_guest::{Limits, ToolContext, tools::fs as tfs};
use sandcrab_protocol::{
    EntryKind, FileDeleteParams, FileEditParams, FileMoveParams, FileReadParams, FileWriteParams,
    ProtocolError,
};
use tempfile::TempDir;

fn setup() -> (TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    (dir, ctx)
}

fn setup_with(limits: Limits) -> (TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::with_limits(dir.path(), limits);
    (dir, ctx)
}

fn read(path: &str) -> FileReadParams {
    FileReadParams {
        path: path.into(),
        offset: None,
        limit: None,
    }
}

fn write(path: &str, content: &str) -> FileWriteParams {
    FileWriteParams {
        path: path.into(),
        content: content.into(),
        append: false,
    }
}

fn edit(path: &str, old: &str, new: &str) -> FileEditParams {
    FileEditParams {
        path: path.into(),
        old_string: old.into(),
        new_string: new.into(),
        replace_all: false,
    }
}

/// Recursively assert no atomic-write temp files are left under `dir`.
fn assert_no_temp_files(dir: &Path) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(!name.contains("crabd-"), "leftover temp file {name}");
        if entry.file_type().unwrap().is_dir() {
            assert_no_temp_files(&entry.path());
        }
    }
}

// ----------------------------------------------------------------------------- file_read

#[tokio::test]
async fn read_numbers_lines() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let r = tfs::file_read(&ctx, read("a.txt")).await.unwrap();
    assert_eq!(r.content, "     1\tone\n     2\ttwo\n     3\tthree");
    assert_eq!((r.total_lines, r.start_line, r.end_line), (3, 1, 3));
    assert!(!r.truncated && !r.is_binary);
}

#[tokio::test]
async fn read_last_line_without_newline_and_empty_file() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("a"), "x\ny").unwrap();
    let r = tfs::file_read(&ctx, read("a")).await.unwrap();
    assert_eq!(r.total_lines, 2);
    assert_eq!(r.content, "     1\tx\n     2\ty");

    fs::write(dir.path().join("empty"), "").unwrap();
    let r = tfs::file_read(&ctx, read("empty")).await.unwrap();
    assert_eq!((r.total_lines, r.start_line, r.end_line), (0, 1, 0));
    assert_eq!(r.content, "");
    assert!(!r.truncated);
}

#[tokio::test]
async fn read_offset_limit_paging() {
    let (dir, ctx) = setup();
    let body: String = (1..=10).map(|i| format!("l{i}\n")).collect();
    fs::write(dir.path().join("a"), body).unwrap();
    let r = tfs::file_read(
        &ctx,
        FileReadParams {
            path: "a".into(),
            offset: Some(4),
            limit: Some(3),
        },
    )
    .await
    .unwrap();
    assert_eq!(r.content, "     4\tl4\n     5\tl5\n     6\tl6");
    assert_eq!((r.total_lines, r.start_line, r.end_line), (10, 4, 6));
    assert!(r.truncated);

    // Past the end: nothing returned, not truncated.
    let r = tfs::file_read(
        &ctx,
        FileReadParams {
            path: "a".into(),
            offset: Some(20),
            limit: None,
        },
    )
    .await
    .unwrap();
    assert_eq!((r.content.as_str(), r.start_line, r.end_line), ("", 20, 19));
    assert!(!r.truncated);

    // The last page is not truncated.
    let r = tfs::file_read(
        &ctx,
        FileReadParams {
            path: "a".into(),
            offset: Some(8),
            limit: Some(5),
        },
    )
    .await
    .unwrap();
    assert_eq!((r.start_line, r.end_line), (8, 10));
    assert!(!r.truncated);
}

#[tokio::test]
async fn read_invalid_offset_limit() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("a"), "x").unwrap();
    for (offset, limit) in [(Some(0), None), (None, Some(0))] {
        let e = tfs::file_read(
            &ctx,
            FileReadParams {
                path: "a".into(),
                offset,
                limit,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(e, ProtocolError::InvalidParams(_)), "{e:?}");
    }
}

#[tokio::test]
async fn read_default_limit_truncates_large_file() {
    let (dir, ctx) = setup_with(Limits {
        file_read_default_lines: 100,
        max_file_bytes: 1024, // streaming read is not capped by max_file_bytes
        ..Limits::default()
    });
    let body: String = (1..=50_000).map(|i| format!("line number {i}\n")).collect();
    fs::write(dir.path().join("big.log"), body).unwrap();
    let r = tfs::file_read(&ctx, read("big.log")).await.unwrap();
    assert_eq!((r.total_lines, r.start_line, r.end_line), (50_000, 1, 100));
    assert!(r.truncated);
    assert_eq!(r.content.lines().count(), 100);
    let r = tfs::file_read(
        &ctx,
        FileReadParams {
            path: "big.log".into(),
            offset: Some(49_999),
            limit: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        r.content,
        " 49999\tline number 49999\n 50000\tline number 50000"
    );
}

#[tokio::test]
async fn read_cuts_long_lines() {
    let (dir, ctx) = setup_with(Limits {
        file_read_max_line_chars: 10,
        ..Limits::default()
    });
    let long = "é".repeat(100_000);
    fs::write(dir.path().join("a"), format!("short\n{long}\nend\n")).unwrap();
    let r = tfs::file_read(&ctx, read("a")).await.unwrap();
    let lines: Vec<&str> = r.content.lines().collect();
    assert_eq!(
        lines[1],
        format!("     2\t{}… [line truncated]", "é".repeat(10))
    );
    assert_eq!(lines[2], "     3\tend");
    assert!(r.truncated);
    assert_eq!(r.end_line, r.total_lines);
}

#[tokio::test]
async fn read_binary_detected() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("nul.bin"), b"abc\0def\n").unwrap();
    fs::write(dir.path().join("bad.bin"), [0xff, 0xfe, 0x00, 0x41]).unwrap();
    fs::write(dir.path().join("latin1"), b"caf\xe9\n").unwrap();
    for name in ["nul.bin", "bad.bin", "latin1"] {
        let r = tfs::file_read(&ctx, read(name)).await.unwrap();
        assert!(r.is_binary, "{name}");
        assert_eq!(r.content, "");
    }
}

#[tokio::test]
async fn read_unicode_and_crlf() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("u.txt"), "héllo 🦀\r\nwörld\r\n").unwrap();
    let r = tfs::file_read(&ctx, read("u.txt")).await.unwrap();
    assert!(!r.is_binary);
    assert_eq!(r.content, "     1\théllo 🦀\n     2\twörld");
}

#[tokio::test]
async fn read_errors() {
    let (dir, ctx) = setup();
    let e = tfs::file_read(&ctx, read("missing")).await.unwrap_err();
    assert!(matches!(e, ProtocolError::NotFound(_)), "{e:?}");
    fs::create_dir(dir.path().join("d")).unwrap();
    let e = tfs::file_read(&ctx, read("d")).await.unwrap_err();
    match e {
        ProtocolError::ExecutionFailed(m) => assert!(m.contains("list_dir"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn read_absolute_path_and_symlink() {
    let (dir, ctx) = setup();
    let f = dir.path().join("t.txt");
    fs::write(&f, "hi\n").unwrap();
    symlink(&f, dir.path().join("link")).unwrap();
    let r = tfs::file_read(&ctx, read(f.to_str().unwrap()))
        .await
        .unwrap();
    assert_eq!(r.content, "     1\thi");
    let r = tfs::file_read(&ctx, read("link")).await.unwrap();
    assert_eq!(r.content, "     1\thi");
}

// ---------------------------------------------------------------------------- file_write

#[tokio::test]
async fn write_creates_parents_and_reports() {
    let (dir, ctx) = setup();
    let r = tfs::file_write(&ctx, write("a/b/c.txt", "héllo\n"))
        .await
        .unwrap();
    let abs = dir.path().join("a/b/c.txt");
    assert_eq!(r.path, abs.to_str().unwrap());
    assert_eq!(r.bytes_written, "héllo\n".len() as u64);
    assert!(r.created);
    assert_eq!(fs::read_to_string(&abs).unwrap(), "héllo\n");
    assert_eq!(
        fs::metadata(&abs).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert_no_temp_files(dir.path());
}

#[tokio::test]
async fn write_overwrite_preserves_mode() {
    let (dir, ctx) = setup();
    let f = dir.path().join("run.sh");
    fs::write(&f, "old").unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o751)).unwrap();
    let ino_before = fs::metadata(&f).unwrap().ino();
    let r = tfs::file_write(&ctx, write("run.sh", "#!/bin/sh\n"))
        .await
        .unwrap();
    assert!(!r.created);
    assert_eq!(fs::read_to_string(&f).unwrap(), "#!/bin/sh\n");
    let meta = fs::metadata(&f).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o751);
    // Replaced via rename → new inode.
    assert_ne!(meta.ino(), ino_before);
    assert_no_temp_files(dir.path());
}

#[tokio::test]
async fn write_append() {
    let (dir, ctx) = setup();
    let mut p = write("log.txt", "a\n");
    p.append = true;
    let r = tfs::file_write(&ctx, p.clone()).await.unwrap();
    assert!(r.created);
    p.content = "b\n".into();
    let r = tfs::file_write(&ctx, p).await.unwrap();
    assert!(!r.created);
    assert_eq!(r.bytes_written, 2);
    assert_eq!(
        fs::read_to_string(dir.path().join("log.txt")).unwrap(),
        "a\nb\n"
    );
}

#[tokio::test]
async fn write_through_symlink_keeps_link() {
    let (dir, ctx) = setup();
    let target = dir.path().join("real.txt");
    fs::write(&target, "old").unwrap();
    symlink("real.txt", dir.path().join("link")).unwrap();
    tfs::file_write(&ctx, write("link", "new")).await.unwrap();
    assert!(
        fs::symlink_metadata(dir.path().join("link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "new");
}

#[tokio::test]
async fn write_errors_leave_no_temp_files() {
    let (dir, ctx) = setup();
    fs::create_dir(dir.path().join("d")).unwrap();
    let e = tfs::file_write(&ctx, write("d", "x")).await.unwrap_err();
    assert!(matches!(e, ProtocolError::ExecutionFailed(_)), "{e:?}");

    // Read-only directory: the temp file cannot be created (skip when running as root).
    let ro = dir.path().join("ro");
    fs::create_dir(&ro).unwrap();
    fs::write(ro.join("f"), "keep").unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::File::create(ro.join("probe")).is_err() {
        let e = tfs::file_write(&ctx, write("ro/f", "new"))
            .await
            .unwrap_err();
        assert!(matches!(e, ProtocolError::ExecutionFailed(_)), "{e:?}");
        assert_eq!(fs::read_to_string(ro.join("f")).unwrap(), "keep");
    }
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    assert_no_temp_files(dir.path());
}

// ----------------------------------------------------------------------------- file_edit

#[tokio::test]
async fn edit_unique_match_with_diff() {
    let (dir, ctx) = setup();
    let f = dir.path().join("m.rs");
    let body: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    fs::write(&f, body).unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
    let r = tfs::file_edit(&ctx, edit("m.rs", "line 10\n", "LINE TEN\n"))
        .await
        .unwrap();
    assert_eq!(r.replacements, 1);
    assert!(r.diff.contains("-line 10\n"), "{}", r.diff);
    assert!(r.diff.contains("+LINE TEN\n"), "{}", r.diff);
    assert!(r.diff.contains("@@ -7,7 +7,7 @@"), "{}", r.diff);
    assert!(
        !r.diff.contains("line 3\n"),
        "context is 3 lines: {}",
        r.diff
    );
    let now = fs::read_to_string(&f).unwrap();
    assert!(now.contains("LINE TEN\nline 11\n"));
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_no_temp_files(dir.path());
}

#[tokio::test]
async fn edit_ambiguous_leaves_file_untouched() {
    let (dir, ctx) = setup();
    let f = dir.path().join("a.txt");
    fs::write(&f, "x = 1\ny = 2\nx = 1\nx = 1\n").unwrap();
    let e = tfs::file_edit(&ctx, edit("a.txt", "x = 1", "x = 9"))
        .await
        .unwrap_err();
    match e {
        ProtocolError::AmbiguousEdit { path, occurrences } => {
            assert_eq!(occurrences, 3);
            assert_eq!(path, f.to_str().unwrap());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        fs::read_to_string(&f).unwrap(),
        "x = 1\ny = 2\nx = 1\nx = 1\n"
    );

    let mut p = edit("a.txt", "x = 1", "x = 9");
    p.replace_all = true;
    let r = tfs::file_edit(&ctx, p).await.unwrap();
    assert_eq!(r.replacements, 3);
    assert_eq!(
        fs::read_to_string(&f).unwrap(),
        "x = 9\ny = 2\nx = 9\nx = 9\n"
    );
    assert_no_temp_files(dir.path());
}

#[tokio::test]
async fn edit_not_found_hints_line() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("a.py"), "def f():\n    return 1\n").unwrap();
    let e = tfs::file_edit(&ctx, edit("a.py", "def f():\n  return 1", "x"))
        .await
        .unwrap_err();
    match e {
        ProtocolError::NotFound(m) => {
            assert!(m.contains("line(s) 1"), "{m}");
            assert!(m.contains("file_read"), "{m}");
        }
        other => panic!("{other:?}"),
    }
    let e = tfs::file_edit(&ctx, edit("missing.py", "a", "b"))
        .await
        .unwrap_err();
    assert!(matches!(e, ProtocolError::NotFound(_)), "{e:?}");
}

#[tokio::test]
async fn edit_invalid_params() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("a"), "abc").unwrap();
    for (old, new) in [("", "x"), ("abc", "abc")] {
        let e = tfs::file_edit(&ctx, edit("a", old, new)).await.unwrap_err();
        assert!(matches!(e, ProtocolError::InvalidParams(_)), "{e:?}");
    }
    assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), "abc");
}

#[tokio::test]
async fn edit_crlf_preserved() {
    let (dir, ctx) = setup();
    let f = dir.path().join("w.txt");
    fs::write(&f, "alpha\r\nbeta\r\ngamma\r\n").unwrap();
    // LF-only old/new (as copied from `file_read` output) still match and keep CRLF.
    let r = tfs::file_edit(&ctx, edit("w.txt", "alpha\nbeta", "one\ntwo"))
        .await
        .unwrap();
    assert_eq!(r.replacements, 1);
    assert_eq!(fs::read_to_string(&f).unwrap(), "one\r\ntwo\r\ngamma\r\n");
    // Single-line edits match exactly.
    tfs::file_edit(&ctx, edit("w.txt", "gamma", "three"))
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(&f).unwrap(), "one\r\ntwo\r\nthree\r\n");
}

#[tokio::test]
async fn edit_unicode() {
    let (dir, ctx) = setup();
    let f = dir.path().join("u.txt");
    fs::write(&f, "naïve café 🦀\n").unwrap();
    let r = tfs::file_edit(&ctx, edit("u.txt", "café 🦀", "crème 🦞"))
        .await
        .unwrap();
    assert_eq!(r.replacements, 1);
    assert_eq!(fs::read_to_string(&f).unwrap(), "naïve crème 🦞\n");
}

#[tokio::test]
async fn edit_binary_dir_and_too_large() {
    let (dir, ctx) = setup_with(Limits {
        max_file_bytes: 16,
        ..Limits::default()
    });
    fs::write(dir.path().join("b"), [0xffu8, 0xfe, b'a']).unwrap();
    let e = tfs::file_edit(&ctx, edit("b", "a", "b")).await.unwrap_err();
    assert!(matches!(e, ProtocolError::ExecutionFailed(_)), "{e:?}");
    fs::create_dir(dir.path().join("d")).unwrap();
    let e = tfs::file_edit(&ctx, edit("d", "a", "b")).await.unwrap_err();
    assert!(matches!(e, ProtocolError::ExecutionFailed(_)), "{e:?}");
    fs::write(dir.path().join("big"), "a".repeat(17)).unwrap();
    let e = tfs::file_edit(&ctx, edit("big", "a", "b"))
        .await
        .unwrap_err();
    assert_eq!(
        e,
        ProtocolError::TooLarge {
            size: 17,
            limit: 16
        }
    );
}

#[tokio::test]
async fn edit_through_symlink_keeps_link() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("real"), "abc").unwrap();
    symlink("real", dir.path().join("link")).unwrap();
    tfs::file_edit(&ctx, edit("link", "b", "B")).await.unwrap();
    assert!(
        fs::symlink_metadata(dir.path().join("link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(dir.path().join("real")).unwrap(), "aBc");
}

// --------------------------------------------------------------------------- file_delete

fn del(path: &str, recursive: bool) -> FileDeleteParams {
    FileDeleteParams {
        path: path.into(),
        recursive,
    }
}

#[tokio::test]
async fn delete_file_and_dir() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("f"), "x").unwrap();
    let r = tfs::file_delete(&ctx, del("f", false)).await.unwrap();
    assert_eq!(r.kind, EntryKind::File);
    assert_eq!(r.path, dir.path().join("f").to_str().unwrap());
    assert!(!dir.path().join("f").exists());

    fs::create_dir_all(dir.path().join("d/sub")).unwrap();
    fs::write(dir.path().join("d/sub/x"), "x").unwrap();
    let e = tfs::file_delete(&ctx, del("d", false)).await.unwrap_err();
    match e {
        ProtocolError::ExecutionFailed(m) => assert!(m.contains("recursive"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(dir.path().join("d/sub/x").exists());
    let r = tfs::file_delete(&ctx, del("d", true)).await.unwrap();
    assert_eq!(r.kind, EntryKind::Dir);
    assert!(!dir.path().join("d").exists());
}

#[tokio::test]
async fn delete_errors() {
    let (_dir, ctx) = setup();
    let e = tfs::file_delete(&ctx, del("missing", true))
        .await
        .unwrap_err();
    assert!(matches!(e, ProtocolError::NotFound(_)), "{e:?}");
    let e = tfs::file_delete(&ctx, del("/", true)).await.unwrap_err();
    assert!(matches!(e, ProtocolError::PathNotAllowed(_)), "{e:?}");
    let e = tfs::file_delete(&ctx, del("../../../../../../..", true))
        .await
        .unwrap_err();
    assert!(matches!(e, ProtocolError::PathNotAllowed(_)), "{e:?}");
}

#[tokio::test]
async fn delete_symlink_never_follows() {
    let (dir, ctx) = setup();
    let target = dir.path().join("precious");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep"), "x").unwrap();
    symlink(&target, dir.path().join("link")).unwrap();

    // Even with recursive (and a trailing slash), only the link goes.
    let r = tfs::file_delete(&ctx, del("link/", true)).await.unwrap();
    assert_eq!(r.kind, EntryKind::Symlink);
    assert!(fs::symlink_metadata(dir.path().join("link")).is_err());
    assert!(target.join("keep").exists());

    // A link inside a recursively deleted dir is not followed either.
    fs::create_dir(dir.path().join("tree")).unwrap();
    symlink(&target, dir.path().join("tree/inner")).unwrap();
    tfs::file_delete(&ctx, del("tree", true)).await.unwrap();
    assert!(target.join("keep").exists());

    // Dangling links are deletable.
    symlink("nowhere", dir.path().join("dangling")).unwrap();
    let r = tfs::file_delete(&ctx, del("dangling", false))
        .await
        .unwrap();
    assert_eq!(r.kind, EntryKind::Symlink);
}

// ----------------------------------------------------------------------------- file_move

fn mv(from: &str, to: &str, overwrite: bool) -> FileMoveParams {
    FileMoveParams {
        from: from.into(),
        to: to.into(),
        overwrite,
    }
}

#[tokio::test]
async fn move_file_creates_parents() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("a"), "A").unwrap();
    let r = tfs::file_move(&ctx, mv("a", "x/y/b", false)).await.unwrap();
    assert_eq!(r.from, dir.path().join("a").to_str().unwrap());
    assert_eq!(r.to, dir.path().join("x/y/b").to_str().unwrap());
    assert!(!dir.path().join("a").exists());
    assert_eq!(fs::read_to_string(dir.path().join("x/y/b")).unwrap(), "A");
}

#[tokio::test]
async fn move_overwrite_semantics() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("a"), "A").unwrap();
    fs::write(dir.path().join("b"), "B").unwrap();
    let e = tfs::file_move(&ctx, mv("a", "b", false)).await.unwrap_err();
    match e {
        ProtocolError::ExecutionFailed(m) => assert!(m.contains("overwrite"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(fs::read_to_string(dir.path().join("b")).unwrap(), "B");
    tfs::file_move(&ctx, mv("a", "b", true)).await.unwrap();
    assert_eq!(fs::read_to_string(dir.path().join("b")).unwrap(), "A");

    // Directory over existing non-empty directory.
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/new"), "n").unwrap();
    fs::create_dir_all(dir.path().join("dst")).unwrap();
    fs::write(dir.path().join("dst/old"), "o").unwrap();
    tfs::file_move(&ctx, mv("src", "dst", true)).await.unwrap();
    assert!(dir.path().join("dst/new").exists());
    assert!(!dir.path().join("dst/old").exists());
}

#[tokio::test]
async fn move_errors() {
    let (dir, ctx) = setup();
    let e = tfs::file_move(&ctx, mv("missing", "x", false))
        .await
        .unwrap_err();
    assert!(matches!(e, ProtocolError::NotFound(_)), "{e:?}");
    fs::create_dir_all(dir.path().join("d/sub")).unwrap();
    let e = tfs::file_move(&ctx, mv("d", "d/sub/inner", false))
        .await
        .unwrap_err();
    assert!(matches!(e, ProtocolError::InvalidParams(_)), "{e:?}");
    let e = tfs::file_move(&ctx, mv("d/sub", "d", true))
        .await
        .unwrap_err();
    assert!(matches!(e, ProtocolError::InvalidParams(_)), "{e:?}");
    assert!(dir.path().join("d/sub").exists());
    // Same path is a no-op.
    tfs::file_move(&ctx, mv("d", "./d", false)).await.unwrap();
    assert!(dir.path().join("d/sub").exists());
}

#[tokio::test]
async fn move_symlink_moves_link() {
    let (dir, ctx) = setup();
    fs::write(dir.path().join("t"), "T").unwrap();
    symlink("t", dir.path().join("l")).unwrap();
    tfs::file_move(&ctx, mv("l", "l2", false)).await.unwrap();
    assert!(
        fs::symlink_metadata(dir.path().join("l2"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(dir.path().join("t").exists());
}

/// Real cross-device move when the host offers a second filesystem (`/dev/shm`).
#[tokio::test]
async fn move_cross_device_if_available() {
    let shm = Path::new("/dev/shm");
    let Ok(other) = tempfile::tempdir_in(shm) else {
        eprintln!("skipping: /dev/shm not available");
        return;
    };
    let (dir, ctx) = setup();
    if fs::metadata(other.path()).unwrap().dev() == fs::metadata(dir.path()).unwrap().dev() {
        eprintln!("skipping: /dev/shm is on the same device as the temp dir");
        return;
    }
    // File.
    fs::write(dir.path().join("f"), "file").unwrap();
    let dest = other.path().join("nested/f");
    tfs::file_move(&ctx, mv("f", dest.to_str().unwrap(), false))
        .await
        .unwrap();
    assert!(!dir.path().join("f").exists());
    assert_eq!(fs::read_to_string(&dest).unwrap(), "file");
    // Directory with a symlink, over an existing destination file.
    fs::create_dir_all(dir.path().join("tree/sub")).unwrap();
    fs::write(dir.path().join("tree/sub/a"), "A").unwrap();
    symlink("sub/a", dir.path().join("tree/ln")).unwrap();
    let dest = other.path().join("tree");
    fs::write(&dest, "in the way").unwrap();
    tfs::file_move(&ctx, mv("tree", dest.to_str().unwrap(), true))
        .await
        .unwrap();
    assert!(!dir.path().join("tree").exists());
    assert_eq!(fs::read_to_string(dest.join("sub/a")).unwrap(), "A");
    assert_eq!(fs::read_to_string(dest.join("ln")).unwrap(), "A");
    assert_no_temp_files(other.path());
}
