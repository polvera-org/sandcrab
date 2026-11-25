//! `list_dir` against fixture trees.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use sandcrab_guest::tools::search::list_dir;
use sandcrab_guest::{Limits, ToolContext};
use sandcrab_protocol::{EntryKind, ListDirParams, ProtocolError};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn params(path: Option<&str>, depth: Option<u64>, respect: bool) -> ListDirParams {
    ListDirParams {
        path: path.map(Into::into),
        depth,
        respect_gitignore: respect,
    }
}

fn paths(ctx: &ToolContext, p: ListDirParams) -> Vec<String> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let r = rt.block_on(list_dir(ctx, p)).unwrap();
    r.entries.into_iter().map(|e| e.path).collect()
}

/// a/            (dir)
/// a/inner.txt
/// b.txt
/// .hidden
/// .gitignore    (ignores `ignored.log` and `build/`)
/// ignored.log
/// build/out.o
/// .git/HEAD
fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(r, "a/inner.txt", "x");
    write(r, "b.txt", "hello");
    write(r, ".hidden", "");
    write(r, ".gitignore", "ignored.log\nbuild/\n");
    write(r, "ignored.log", "");
    write(r, "build/out.o", "");
    write(r, ".git/HEAD", "ref: refs/heads/main\n");
    d
}

#[test]
fn default_depth_one_dirs_first_hidden_shown_git_skipped() {
    let d = fixture();
    let ctx = ToolContext::new(d.path());
    assert_eq!(
        paths(&ctx, params(None, None, true)),
        ["a", ".gitignore", ".hidden", "b.txt"]
    );
}

#[test]
fn gitignore_can_be_disabled() {
    let d = fixture();
    let ctx = ToolContext::new(d.path());
    assert_eq!(
        paths(&ctx, params(None, Some(1), false)),
        [
            "a",
            "build",
            ".gitignore",
            ".hidden",
            "b.txt",
            "ignored.log"
        ]
    );
}

#[test]
fn depth_is_depth_first_and_limited() {
    let d = fixture();
    write(d.path(), "a/deep/deeper/z.txt", "");
    let ctx = ToolContext::new(d.path());
    assert_eq!(
        paths(&ctx, params(None, Some(2), true)),
        [
            "a",
            "a/deep",
            "a/inner.txt",
            ".gitignore",
            ".hidden",
            "b.txt"
        ]
    );
    let all = paths(&ctx, params(None, Some(10), true));
    assert!(all.contains(&"a/deep/deeper/z.txt".to_string()));
}

#[test]
fn depth_zero_is_invalid() {
    let d = fixture();
    let ctx = ToolContext::new(d.path());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt
        .block_on(list_dir(&ctx, params(None, Some(0), true)))
        .unwrap_err();
    assert!(matches!(err, ProtocolError::InvalidParams(_)), "{err:?}");
}

#[test]
fn nested_gitignore_and_dot_ignore() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(r, "sub/.gitignore", "secret.txt\n");
    write(r, "sub/secret.txt", "");
    write(r, "sub/keep.txt", "");
    write(r, "secret.txt", ""); // not covered by sub/.gitignore
    write(r, ".ignore", "skipme\n");
    write(r, "skipme", "");
    let ctx = ToolContext::new(r);
    assert_eq!(
        paths(&ctx, params(None, Some(2), true)),
        [
            "sub",
            "sub/.gitignore",
            "sub/keep.txt",
            ".ignore",
            "secret.txt"
        ]
    );
}

#[test]
fn entry_metadata() {
    let d = fixture();
    let r = d.path();
    fs::set_permissions(r.join("b.txt"), fs::Permissions::from_mode(0o640)).unwrap();
    std::os::unix::fs::symlink("b.txt", r.join("link")).unwrap();
    let ctx = ToolContext::new(r);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let res = rt
        .block_on(list_dir(&ctx, params(Some("."), None, true)))
        .unwrap();
    let get = |p: &str| res.entries.iter().find(|e| e.path == p).unwrap().clone();
    let b = get("b.txt");
    assert_eq!((b.kind, b.size, b.mode), (EntryKind::File, 5, 0o640));
    let a = get("a");
    assert_eq!((a.kind, a.size), (EntryKind::Dir, 0));
    assert_eq!(get("link").kind, EntryKind::Symlink);
    assert!(!res.truncated);
}

#[test]
fn symlinked_dirs_not_followed() {
    let d = fixture();
    std::os::unix::fs::symlink(d.path().join("a"), d.path().join("alink")).unwrap();
    let ctx = ToolContext::new(d.path());
    let all = paths(&ctx, params(None, Some(5), true));
    assert!(all.contains(&"alink".to_string()));
    assert!(!all.iter().any(|p| p.starts_with("alink/")), "{all:?}");
}

#[test]
fn cap_sets_truncated() {
    let d = fixture();
    let limits = Limits {
        list_dir_max_entries: 2,
        ..Limits::default()
    };
    let ctx = ToolContext::with_limits(d.path(), limits);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let res = rt
        .block_on(list_dir(&ctx, params(None, None, true)))
        .unwrap();
    assert_eq!(res.entries.len(), 2);
    assert!(res.truncated);
}

#[test]
fn missing_root_and_file_root() {
    let d = fixture();
    let ctx = ToolContext::new(d.path());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt
        .block_on(list_dir(&ctx, params(Some("nope"), None, true)))
        .unwrap_err();
    assert!(matches!(err, ProtocolError::NotFound(_)), "{err:?}");
    let err = rt
        .block_on(list_dir(&ctx, params(Some("b.txt"), None, true)))
        .unwrap_err();
    assert!(matches!(err, ProtocolError::ExecutionFailed(_)), "{err:?}");
}

#[test]
fn unicode_names() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "ünïcødé/日本.txt", "");
    let ctx = ToolContext::new(d.path());
    assert_eq!(
        paths(&ctx, params(None, Some(2), true)),
        ["ünïcødé", "ünïcødé/日本.txt"]
    );
}
