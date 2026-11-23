//! `bash` tool tests against real processes (Linux / Unix).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use sandcrab_guest::tools::exec::bash;
use sandcrab_guest::{Limits, ToolContext};
use sandcrab_protocol::{BashParams, ProtocolError};

fn params(cmd: &str) -> BashParams {
    BashParams {
        command: cmd.to_owned(),
        cwd: None,
        env: BTreeMap::new(),
        timeout_ms: None,
    }
}

fn ctx_in(dir: &std::path::Path) -> ToolContext {
    ToolContext::new(dir)
}

/// True once `pid` is gone or a zombie.
fn pid_dead(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

async fn wait_dead(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if pid_dead(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn pids(stdout: &str) -> Vec<u32> {
    stdout
        .split_whitespace()
        .filter_map(|w| w.parse().ok())
        .collect()
}

#[tokio::test]
async fn exit_code_and_streams() {
    let dir = tempfile::tempdir().unwrap();
    let r = bash(
        &ctx_in(dir.path()),
        params("echo out; echo err >&2; exit 3"),
    )
    .await
    .unwrap();
    assert_eq!(r.exit_code, Some(3));
    assert_eq!(r.signal, None);
    assert_eq!(r.stdout, "out\n");
    assert_eq!(r.stderr, "err\n");
    assert!(!r.truncated && !r.timed_out);
}

#[tokio::test]
async fn killed_by_signal_reports_signal() {
    let dir = tempfile::tempdir().unwrap();
    let r = bash(&ctx_in(dir.path()), params("kill -SEGV $$"))
        .await
        .unwrap();
    assert_eq!(r.exit_code, None);
    assert_eq!(r.signal, Some(11));
    assert!(!r.timed_out);
}

#[tokio::test]
async fn env_and_cwd() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let mut p = params("echo \"$FOO\"; pwd");
    p.env.insert("FOO".into(), "bar baz".into());
    p.cwd = Some("sub".into());
    let r = bash(&ctx_in(dir.path()), p).await.unwrap();
    let real = std::fs::canonicalize(dir.path().join("sub")).unwrap();
    assert_eq!(r.stdout, format!("bar baz\n{}\n", real.display()));

    // Default cwd, and the daemon env is inherited.
    let r = bash(&ctx_in(dir.path()), params("pwd; echo ${PATH:+has-path}"))
        .await
        .unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(r.stdout, format!("{}\nhas-path\n", real.display()));
}

#[tokio::test]
async fn missing_cwd_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = params("true");
    p.cwd = Some("nope".into());
    let e = bash(&ctx_in(dir.path()), p).await.unwrap_err();
    assert!(matches!(e, ProtocolError::NotFound(_)), "{e:?}");
}

#[tokio::test]
async fn stdin_is_dev_null() {
    let dir = tempfile::tempdir().unwrap();
    let r = bash(&ctx_in(dir.path()), params("cat; echo done"))
        .await
        .unwrap();
    assert_eq!(r.stdout, "done\n");
}

#[tokio::test]
async fn invalid_utf8_is_lossy() {
    let dir = tempfile::tempdir().unwrap();
    let r = bash(&ctx_in(dir.path()), params("printf 'a\\xffb'"))
        .await
        .unwrap();
    assert_eq!(r.stdout, "a\u{fffd}b");
}

#[tokio::test]
async fn timeout_kills_group_including_grandchildren() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = params("sleep 1000 & echo $!; (sleep 1000; true) & echo $!; wait");
    p.timeout_ms = Some(500);
    let start = Instant::now();
    let r = bash(&ctx_in(dir.path()), p).await.unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert!(r.timed_out);
    assert_eq!(r.exit_code, None);
    assert_eq!(
        r.signal,
        Some(15),
        "bash dies of the SIGTERM sent to its group"
    );
    let ps = pids(&r.stdout);
    assert_eq!(ps.len(), 2, "{r:?}");
    for pid in ps {
        assert!(
            wait_dead(pid).await,
            "grandchild {pid} survived the timeout"
        );
    }
}

#[tokio::test]
async fn timeout_escalates_to_sigkill_when_term_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = params("trap '' TERM; echo ready; while :; do sleep 0.05; done");
    p.timeout_ms = Some(300);
    let start = Instant::now();
    let r = bash(&ctx_in(dir.path()), p).await.unwrap();
    assert!(r.timed_out);
    assert_eq!(r.signal, Some(9));
    assert_eq!(r.stdout, "ready\n", "output so far is returned");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn timeout_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        bash_max_timeout_ms: 200,
        ..Limits::default()
    };
    let ctx = ToolContext::with_limits(dir.path(), limits);
    let mut p = params("sleep 1000");
    p.timeout_ms = Some(10_000_000);
    let r = bash(&ctx, p).await.unwrap();
    assert!(r.timed_out);
    assert!(r.duration_ms < 3_000, "{}", r.duration_ms);
}

#[tokio::test]
async fn background_stragglers_are_killed_on_normal_exit() {
    let dir = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let r = bash(&ctx_in(dir.path()), params("sleep 1000 & echo $!"))
        .await
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(r.exit_code, Some(0));
    let pid = pids(&r.stdout)[0];
    assert!(wait_dead(pid).await, "backgrounded job {pid} survived");
}

#[tokio::test]
async fn truncation_keeps_head_and_tail() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        bash_max_output_bytes: 20,
        ..Limits::default()
    };
    let ctx = ToolContext::with_limits(dir.path(), limits);
    let r = bash(&ctx, params("seq 1 1000; echo short >&2"))
        .await
        .unwrap();
    assert!(r.truncated);
    let full: String = (1..=1000).map(|i| format!("{i}\n")).collect();
    let head = &full[..10];
    let tail = &full[full.len() - 10..];
    let dropped = full.len() - 20;
    assert_eq!(
        r.stdout,
        format!("{head}\n[... {dropped} bytes truncated ...]\n{tail}")
    );
    assert_eq!(r.stderr, "short\n");
}

#[tokio::test]
async fn huge_output_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let r = bash(&ctx_in(dir.path()), params("yes | head -c 50000000"))
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(0));
    assert!(r.truncated);
    let max = Limits::default().bash_max_output_bytes;
    assert!(r.stdout.len() < max + 100, "{}", r.stdout.len());
    assert!(
        r.stdout
            .contains(&format!("[... {} bytes truncated ...]", 50_000_000 - max))
    );
    assert!(r.stdout.starts_with("y\ny\n") && r.stdout.ends_with("y\ny\n"));
}

#[tokio::test]
async fn dropped_future_kills_group() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("pids");
    let ctx = ctx_in(dir.path());
    let cmd = format!(
        "sleep 1000 & echo $! >> {m}; echo $$ >> {m}; wait",
        m = marker.display()
    );
    let fut = bash(&ctx, params(&cmd));
    // Run the command until both pids are recorded, then drop the request future.
    let _ = tokio::time::timeout(Duration::from_millis(500), fut).await;
    let ps = pids(&std::fs::read_to_string(&marker).unwrap());
    assert_eq!(ps.len(), 2);
    for pid in ps {
        assert!(wait_dead(pid).await, "{pid} survived the dropped request");
    }
}
