//! `grep` against fixture trees.

use std::fs;
use std::path::Path;

use sandcrab_guest::tools::search::{GREP_MAX_LINE_CHARS, GREP_MAX_MATCHES_PER_FILE, grep};
use sandcrab_guest::{Limits, ToolContext};
use sandcrab_protocol::{GrepParams, GrepResult, ProtocolError};

fn write(root: &Path, rel: &str, body: impl AsRef<[u8]>) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn p(pattern: &str) -> GrepParams {
    GrepParams {
        pattern: pattern.into(),
        root: None,
        glob: None,
        literal: false,
        case_insensitive: false,
        context_lines: None,
    }
}

fn run(ctx: &ToolContext, params: GrepParams) -> Result<GrepResult, ProtocolError> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(grep(ctx, params))
}

fn hits(r: &GrepResult) -> Vec<(String, u64)> {
    r.matches
        .iter()
        .map(|m| (m.path.clone(), m.line_number))
        .collect()
}

fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(r, "src/a.rs", "fn main() {\n    let needle = 1;\n}\n");
    write(r, "src/b.ts", "const Needle = 2;\nneedle();\n");
    write(r, "z.txt", "needle at top\n");
    write(r, "ignored/x.rs", "needle\n");
    write(r, ".gitignore", "ignored/\n");
    write(r, ".env.example", "NEEDLE=1\nneedle=2\n");
    write(r, ".git/config", "needle\n");
    d
}

#[test]
fn sorted_by_path_then_line_gitignore_and_git_skipped() {
    let d = fixture();
    let ctx = ToolContext::new(d.path());
    let r = run(&ctx, p("needle")).unwrap();
    assert_eq!(
        hits(&r),
        [
            (".env.example".to_string(), 2),
            ("src/a.rs".to_string(), 2),
            ("src/b.ts".to_string(), 2),
            ("z.txt".to_string(), 1),
        ]
    );
    assert_eq!(r.matches[1].line, "    let needle = 1;");
    assert!(!r.truncated);
}

#[test]
fn case_insensitive() {
    let d = fixture();
    let ctx = ToolContext::new(d.path());
    let r = run(
        &ctx,
        GrepParams {
            case_insensitive: true,
            root: Some("src".into()),
            ..p("NEEDLE")
        },
    )
    .unwrap();
    assert_eq!(
        hits(&r),
        [
            ("a.rs".to_string(), 2),
            ("b.ts".to_string(), 1),
            ("b.ts".to_string(), 2)
        ]
    );
}

#[test]
fn regex_vs_literal() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "f.txt", "a.b\naxb\n(a+\n");
    let ctx = ToolContext::new(d.path());
    let regex = run(&ctx, p("a.b")).unwrap();
    assert_eq!(regex.matches.len(), 2);
    let lit = run(
        &ctx,
        GrepParams {
            literal: true,
            ..p("a.b")
        },
    )
    .unwrap();
    assert_eq!(hits(&lit), [("f.txt".to_string(), 1)]);
    // Invalid as regex, fine as literal.
    let err = run(&ctx, p("(a+")).unwrap_err();
    assert!(matches!(err, ProtocolError::InvalidParams(_)), "{err:?}");
    let lit = run(
        &ctx,
        GrepParams {
            literal: true,
            ..p("(a+")
        },
    )
    .unwrap();
    assert_eq!(hits(&lit), [("f.txt".to_string(), 3)]);
}

#[test]
fn glob_filter_basename_and_path() {
    let d = fixture();
    let ctx = ToolContext::new(d.path());
    let by_name = run(
        &ctx,
        GrepParams {
            glob: Some("*.ts".into()),
            ..p("needle")
        },
    )
    .unwrap();
    assert_eq!(hits(&by_name), [("src/b.ts".to_string(), 2)]);
    let by_path = run(
        &ctx,
        GrepParams {
            glob: Some("src/*.rs".into()),
            ..p("needle")
        },
    )
    .unwrap();
    assert_eq!(hits(&by_path), [("src/a.rs".to_string(), 2)]);
    let err = run(
        &ctx,
        GrepParams {
            glob: Some("[".into()),
            ..p("needle")
        },
    )
    .unwrap_err();
    assert!(matches!(err, ProtocolError::InvalidParams(_)));
}

#[test]
fn context_lines_and_file_edges() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "f.txt", "hit 1\nb\nc\nd\nhit 5\nf\nhit 7\n");
    let ctx = ToolContext::new(d.path());
    let r = run(
        &ctx,
        GrepParams {
            context_lines: Some(2),
            ..p("hit")
        },
    )
    .unwrap();
    let m = &r.matches;
    assert_eq!(m.len(), 3);
    // Start of file: no before-context.
    assert!(m[0].context_before.is_empty());
    assert_eq!(m[0].context_after, ["b", "c"]);
    assert_eq!(m[1].context_before, ["c", "d"]);
    // Overlapping context is repeated per match.
    assert_eq!(m[1].context_after, ["f", "hit 7"]);
    assert_eq!(m[2].context_before, ["hit 5", "f"]);
    // End of file: no after-context.
    assert!(m[2].context_after.is_empty());
}

#[test]
fn single_file_root_and_crlf() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "w.txt", "one\r\nneedle two\r\n");
    let ctx = ToolContext::new(d.path());
    let r = run(
        &ctx,
        GrepParams {
            root: Some("w.txt".into()),
            ..p("needle")
        },
    )
    .unwrap();
    assert_eq!(r.matches.len(), 1);
    assert_eq!(r.matches[0].path, "w.txt");
    assert_eq!(r.matches[0].line, "needle two");
}

#[test]
fn binary_files_skipped() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "bin.dat", b"needle\x00\x01\x02needle\n");
    write(d.path(), "text.txt", "needle\n");
    let ctx = ToolContext::new(d.path());
    let r = run(&ctx, p("needle")).unwrap();
    assert_eq!(hits(&r), [("text.txt".to_string(), 1)]);
}

#[test]
fn unicode_and_long_lines() {
    let d = tempfile::tempdir().unwrap();
    let long = format!("ü{}", "é".repeat(GREP_MAX_LINE_CHARS + 10));
    write(d.path(), "u.txt", format!("日本語のテキスト\n{long}\n"));
    let ctx = ToolContext::new(d.path());
    let r = run(&ctx, p("本語")).unwrap();
    assert_eq!(r.matches[0].line, "日本語のテキスト");
    let r = run(&ctx, p("^ü")).unwrap();
    let line = &r.matches[0].line;
    assert!(line.starts_with('ü'));
    assert!(line.ends_with("… [11 more chars]"), "{line}");
    assert_eq!(
        line.chars().count(),
        GREP_MAX_LINE_CHARS + "… [11 more chars]".len() - 2
    );
}

#[test]
fn total_cap_truncates() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "a.txt", "x\nx\nx\n");
    write(d.path(), "b.txt", "x\nx\n");
    let ctx = ToolContext::with_limits(
        d.path(),
        Limits {
            grep_max_matches: 4,
            ..Limits::default()
        },
    );
    let r = run(&ctx, p("x")).unwrap();
    assert_eq!(r.matches.len(), 4);
    assert!(r.truncated);
    // Cap landing exactly on the last match is not truncation.
    let ctx = ToolContext::with_limits(
        d.path(),
        Limits {
            grep_max_matches: 5,
            ..Limits::default()
        },
    );
    let r = run(&ctx, p("x")).unwrap();
    assert_eq!(r.matches.len(), 5);
    assert!(!r.truncated);
    // Cap reached exactly at a file boundary, more matches in the next file.
    let ctx = ToolContext::with_limits(
        d.path(),
        Limits {
            grep_max_matches: 3,
            ..Limits::default()
        },
    );
    let r = run(&ctx, p("x")).unwrap();
    assert_eq!(r.matches.len(), 3);
    assert!(r.truncated);
}

#[test]
fn per_file_cap_truncates_but_continues() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "a.txt",
        "x\n".repeat(GREP_MAX_MATCHES_PER_FILE + 5),
    );
    write(d.path(), "b.txt", "x\n");
    let ctx = ToolContext::new(d.path());
    let r = run(&ctx, p("x")).unwrap();
    assert_eq!(r.matches.len(), GREP_MAX_MATCHES_PER_FILE + 1);
    assert_eq!(r.matches.last().unwrap().path, "b.txt");
    assert!(r.truncated);
}

#[test]
fn missing_root_not_found() {
    let d = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(d.path());
    let err = run(
        &ctx,
        GrepParams {
            root: Some("nope".into()),
            ..p("x")
        },
    )
    .unwrap_err();
    assert!(matches!(err, ProtocolError::NotFound(_)), "{err:?}");
}
