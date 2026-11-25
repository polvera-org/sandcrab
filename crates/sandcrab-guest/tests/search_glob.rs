//! `glob` against fixture trees.

use std::fs;
use std::path::Path;

use sandcrab_guest::tools::search::glob;
use sandcrab_guest::{Limits, ToolContext};
use sandcrab_protocol::{GlobParams, GlobResult, ProtocolError};

fn write(root: &Path, rel: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, "x").unwrap();
}

fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    for f in [
        "main.rs",
        "src/lib.rs",
        "src/util/mod.rs",
        "src/notes.md",
        "target/debug/gen.rs",
        "vendor/keep/a.rs",
        ".github/workflows/ci.yml",
        ".git/hooks/pre-commit.rs",
    ] {
        write(r, f);
    }
    fs::write(r.join(".gitignore"), "target/\n").unwrap();
    fs::write(r.join("vendor/.gitignore"), "*.rs\n!keep/\n").unwrap();
    d
}

fn run(ctx: &ToolContext, pattern: &str, root: Option<&str>, respect: bool) -> GlobResult {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(glob(
        ctx,
        GlobParams {
            pattern: pattern.into(),
            root: root.map(Into::into),
            respect_gitignore: respect,
        },
    ))
    .unwrap()
}

#[test]
fn recursive_sorted_gitignore_respected() {
    let d = fixture();
    let ctx = ToolContext::new(d.path());
    let r = run(&ctx, "**/*.rs", None, true);
    // vendor/.gitignore ignores `*.rs` at any depth below vendor/.
    assert_eq!(r.paths, ["main.rs", "src/lib.rs", "src/util/mod.rs"]);
    assert!(!r.truncated);
}

#[test]
fn gitignore_disabled_still_skips_git() {
    let d = fixture();
    let ctx = ToolContext::new(d.path());
    let r = run(&ctx, "**/*.rs", None, false);
    assert_eq!(
        r.paths,
        [
            "main.rs",
            "src/lib.rs",
            "src/util/mod.rs",
            "target/debug/gen.rs",
            "vendor/keep/a.rs"
        ]
    );
}

#[test]
fn star_does_not_cross_separator() {
    let d = fixture();
    let ctx = ToolContext::new(d.path());
    assert_eq!(run(&ctx, "*.rs", None, true).paths, ["main.rs"]);
    assert_eq!(run(&ctx, "./src/*.rs", None, true).paths, ["src/lib.rs"]);
    assert_eq!(
        run(&ctx, "src/**/*.{rs,md}", None, true).paths,
        ["src/lib.rs", "src/notes.md", "src/util/mod.rs"]
    );
}

#[test]
fn hidden_files_matched_and_root_param() {
    let d = fixture();
    let ctx = ToolContext::new(d.path());
    assert_eq!(
        run(&ctx, "**/*.yml", None, true).paths,
        [".github/workflows/ci.yml"]
    );
    assert_eq!(
        run(&ctx, "**/*.rs", Some("src"), true).paths,
        ["lib.rs", "util/mod.rs"]
    );
}

#[test]
fn cap_sets_truncated() {
    let d = fixture();
    let limits = Limits {
        glob_max_results: 2,
        ..Limits::default()
    };
    let ctx = ToolContext::with_limits(d.path(), limits);
    let r = run(&ctx, "**/*.rs", None, true);
    assert_eq!(r.paths.len(), 2);
    assert!(r.truncated);
    // Exactly at the cap is not truncated.
    let limits = Limits {
        glob_max_results: 3,
        ..Limits::default()
    };
    let ctx = ToolContext::with_limits(d.path(), limits);
    assert!(!run(&ctx, "**/*.rs", None, true).truncated);
}

#[test]
fn errors() {
    let d = fixture();
    let ctx = ToolContext::new(d.path());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let call = |pattern: &str, root: Option<&str>| {
        rt.block_on(glob(
            &ctx,
            GlobParams {
                pattern: pattern.into(),
                root: root.map(Into::into),
                respect_gitignore: true,
            },
        ))
        .unwrap_err()
    };
    assert!(matches!(
        call("src/[", None),
        ProtocolError::InvalidParams(_)
    ));
    assert!(matches!(call("", None), ProtocolError::InvalidParams(_)));
    assert!(matches!(
        call("*", Some("missing")),
        ProtocolError::NotFound(_)
    ));
    assert!(matches!(
        call("*", Some("main.rs")),
        ProtocolError::ExecutionFailed(_)
    ));
}

#[test]
fn unicode_paths() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "données/résumé.txt");
    let ctx = ToolContext::new(d.path());
    assert_eq!(
        run(&ctx, "**/résumé.*", None, true).paths,
        ["données/résumé.txt"]
    );
}
