//! `process_*` tool tests against real processes (Linux / Unix).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use sandcrab_guest::tools::process::{
    process_kill, process_list, process_poll, process_start, process_stdin,
};
use sandcrab_guest::{Limits, ToolContext};
use sandcrab_protocol::{
    KillSignal, ProcessKillParams, ProcessListParams, ProcessPollParams, ProcessPollResult,
    ProcessStartParams, ProcessStdinParams, ProtocolError,
};

fn start(cmd: &str) -> ProcessStartParams {
    ProcessStartParams {
        command: cmd.to_owned(),
        cwd: None,
        env: BTreeMap::new(),
        pty: false,
    }
}

async fn poll(ctx: &ToolContext, id: &str) -> ProcessPollResult {
    process_poll(
        ctx,
        ProcessPollParams {
            process_id: id.to_owned(),
        },
    )
    .await
    .unwrap()
}

/// Poll until `pred(accumulated stdout, last result)` or 5 s pass; returns accumulated stdout
/// and the last result.
async fn poll_until(
    ctx: &ToolContext,
    id: &str,
    pred: impl Fn(&str, &ProcessPollResult) -> bool,
) -> (String, ProcessPollResult) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut acc = String::new();
    loop {
        let r = poll(ctx, id).await;
        acc.push_str(&r.stdout);
        if pred(&acc, &r) || Instant::now() > deadline {
            return (acc, r);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stdin(ctx: &ToolContext, id: &str, data: &str) -> Result<u64, ProtocolError> {
    process_stdin(
        ctx,
        ProcessStdinParams {
            process_id: id.to_owned(),
            data: data.to_owned(),
        },
    )
    .await
    .map(|r| r.bytes_written)
}

async fn kill(ctx: &ToolContext, id: &str, signal: KillSignal) -> bool {
    process_kill(
        ctx,
        ProcessKillParams {
            process_id: id.to_owned(),
            signal,
        },
    )
    .await
    .unwrap()
    .was_running
}

/// Whether this process inherited an ignored SIGINT (bit 1 of `SigIgn`).
fn sigint_ignored() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("SigIgn:"))
                .and_then(|m| u64::from_str_radix(m.trim(), 16).ok())
        })
        .is_some_and(|mask| mask & (1 << 1) != 0)
}

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

#[tokio::test]
async fn incremental_output_and_exit() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    let s = process_start(
        &ctx,
        start("echo one; echo e >&2; sleep 0.3; echo two; exit 4"),
    )
    .await
    .unwrap();
    assert_eq!(s.process_id, "p-1");
    assert!(s.pid > 0);

    let (first, r) = poll_until(&ctx, &s.process_id, |acc, _| acc.contains("one")).await;
    assert_eq!(first, "one\n");
    assert!(r.running);

    let (rest, r) = poll_until(&ctx, &s.process_id, |_, r| !r.running).await;
    assert_eq!(rest, "two\n", "output since the previous poll only");
    assert_eq!(r.exit_code, Some(4));
    assert_eq!(r.signal, None);

    // Final status stays pollable, with no repeated output.
    let again = poll(&ctx, &s.process_id).await;
    assert!(!again.running && again.stdout.is_empty() && again.exit_code == Some(4));
}

#[tokio::test]
async fn stdin_echo_via_cat() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    let id = process_start(&ctx, start("cat")).await.unwrap().process_id;
    assert_eq!(stdin(&ctx, &id, "hello\n").await.unwrap(), 6);
    let (out, r) = poll_until(&ctx, &id, |acc, _| acc.contains("hello")).await;
    assert_eq!(out, "hello\n");
    assert!(r.running);
    // SIGINT ignored by our parent (e.g. a test run started with `&` from a script) is
    // inherited by every child and cannot be undone by bash; fall back to TERM there.
    let (sig, num) = if sigint_ignored() {
        (KillSignal::Term, 15)
    } else {
        (KillSignal::Int, 2)
    };
    assert!(kill(&ctx, &id, sig).await);
    let (_, r) = poll_until(&ctx, &id, |_, r| !r.running).await;
    assert_eq!(r.signal, Some(num));
    // Exited: stdin errors, kill reports not running.
    let e = stdin(&ctx, &id, "x").await.unwrap_err();
    assert!(matches!(e, ProtocolError::ExecutionFailed(_)), "{e:?}");
    assert!(!kill(&ctx, &id, KillSignal::Kill).await);
}

#[tokio::test]
async fn kill_signals_whole_group() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    let id = process_start(
        &ctx,
        start("sleep 1000 & echo $!; sleep 1000 & echo $!; wait"),
    )
    .await
    .unwrap()
    .process_id;
    let (out, _) = poll_until(&ctx, &id, |acc, _| acc.lines().count() == 2).await;
    // TERM, not INT: non-interactive bash starts `&` jobs with SIGINT ignored (POSIX).
    assert!(kill(&ctx, &id, KillSignal::Term).await);
    let (_, r) = poll_until(&ctx, &id, |_, r| !r.running).await;
    assert!(!r.running);
    for pid in out.split_whitespace().map(|p| p.parse::<u32>().unwrap()) {
        assert!(wait_dead(pid).await, "grandchild {pid} survived");
    }
}

#[tokio::test]
async fn unknown_id_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    let e = process_poll(
        &ctx,
        ProcessPollParams {
            process_id: "p-99".into(),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(e, ProtocolError::NotFound(_)));
}

#[tokio::test]
async fn env_cwd_and_missing_cwd() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("w")).unwrap();
    let ctx = ToolContext::new(dir.path());
    let mut p = start("echo $X; pwd");
    p.env.insert("X".into(), "42".into());
    p.cwd = Some("w".into());
    let id = process_start(&ctx, p).await.unwrap().process_id;
    let (out, _) = poll_until(&ctx, &id, |_, r| !r.running).await;
    let real = std::fs::canonicalize(dir.path().join("w")).unwrap();
    assert_eq!(out, format!("42\n{}\n", real.display()));

    let mut p = start("true");
    p.cwd = Some("missing".into());
    let e = process_start(&ctx, p).await.unwrap_err();
    assert!(matches!(e, ProtocolError::NotFound(_)));
}

#[tokio::test]
async fn buffer_overflow_drops_oldest_and_flags() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        process_max_buffer_bytes: 1000,
        ..Limits::default()
    };
    let ctx = ToolContext::with_limits(dir.path(), limits);
    let id = process_start(&ctx, start("seq 1 100000"))
        .await
        .unwrap()
        .process_id;
    // Let it finish without polling, so everything but the newest 1000 bytes is dropped.
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_list(&ctx, ProcessListParams {})
        .await
        .unwrap()
        .processes[0]
        .running
    {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let r = poll(&ctx, &id).await;
    assert!(r.truncated);
    assert_eq!(r.stdout.len(), 1000);
    assert!(r.stdout.ends_with("99999\n100000\n"), "newest output kept");
    assert!(!poll(&ctx, &id).await.truncated, "flag is reported once");
}

#[tokio::test]
async fn list_and_count_limit_with_gc() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        process_max_count: 2,
        ..Limits::default()
    };
    let ctx = ToolContext::with_limits(dir.path(), limits);
    let a = process_start(&ctx, start("sleep 1000")).await.unwrap();
    let b = process_start(&ctx, start("exit 7")).await.unwrap();
    let list = process_list(&ctx, ProcessListParams {}).await.unwrap();
    let ids: Vec<_> = list
        .processes
        .iter()
        .map(|p| p.process_id.as_str())
        .collect();
    assert_eq!(ids, ["p-1", "p-2"]);
    assert_eq!(list.processes[0].command, "sleep 1000");
    assert_eq!(list.processes[0].pid, a.pid);
    assert!(list.processes[0].running && !list.processes[0].pty);

    // b exits; once it has, starting another evicts it.
    poll_until(&ctx, &b.process_id, |_, r| !r.running).await;
    let c = process_start(&ctx, start("sleep 1000")).await.unwrap();
    assert_eq!(c.process_id, "p-3");
    let list = process_list(&ctx, ProcessListParams {}).await.unwrap();
    let ids: Vec<_> = list
        .processes
        .iter()
        .map(|p| p.process_id.as_str())
        .collect();
    assert_eq!(ids, ["p-1", "p-3"]);

    // All running: the limit is enforced.
    let e = process_start(&ctx, start("true")).await.unwrap_err();
    assert!(matches!(e, ProtocolError::ExecutionFailed(_)), "{e:?}");

    ctx.processes.shutdown().await;
    assert!(wait_dead(a.pid).await && wait_dead(c.pid).await);
}

#[tokio::test]
async fn shutdown_kills_all_even_if_term_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    let a = process_start(&ctx, start("sleep 1000 & echo $!; wait"))
        .await
        .unwrap();
    let b = process_start(&ctx, start("trap '' TERM; while :; do sleep 0.05; done"))
        .await
        .unwrap();
    let (out, _) = poll_until(&ctx, &a.process_id, |acc, _| !acc.is_empty()).await;
    let grandchild: u32 = out.trim().parse().unwrap();
    let t = Instant::now();
    ctx.processes.shutdown().await;
    assert!(t.elapsed() < Duration::from_secs(5));
    for pid in [a.pid, b.pid, grandchild] {
        assert!(wait_dead(pid).await, "{pid} survived shutdown");
    }
    let list = process_list(&ctx, ProcessListParams {}).await.unwrap();
    assert!(list.processes.iter().all(|p| !p.running));
}

#[tokio::test]
async fn pty_process_has_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    let mut p = start("tty; echo \"term=$TERM\"; read -r line; echo \"got:$line\"");
    p.pty = true;
    let s = process_start(&ctx, p).await.unwrap();
    let (out, _) = poll_until(&ctx, &s.process_id, |acc, _| acc.contains("term=")).await;
    // Linux: /dev/pts/N; macOS: /dev/ttysNNN.
    assert!(
        out.starts_with("/dev/pts/") || out.starts_with("/dev/tty"),
        "{out:?}"
    );
    assert!(out.contains("term=xterm-256color"), "{out:?}");
    stdin(&ctx, &s.process_id, "abc\n").await.unwrap();
    let (out, r) = poll_until(&ctx, &s.process_id, |_, r| !r.running).await;
    assert!(out.contains("got:abc"), "{out:?}");
    assert_eq!(r.exit_code, Some(0));
    assert!(r.stderr.is_empty());
    let list = process_list(&ctx, ProcessListParams {}).await.unwrap();
    assert!(list.processes[0].pty);
}

#[tokio::test]
async fn pty_kill_reaches_group() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    let mut p = start("sleep 1000 & echo $!; wait");
    p.pty = true;
    let s = process_start(&ctx, p).await.unwrap();
    let (out, _) = poll_until(&ctx, &s.process_id, |acc, _| acc.contains('\n')).await;
    let gc: u32 = out.trim().parse().unwrap();
    assert!(kill(&ctx, &s.process_id, KillSignal::Kill).await);
    let (_, r) = poll_until(&ctx, &s.process_id, |_, r| !r.running).await;
    assert_eq!(r.signal, Some(9));
    assert!(wait_dead(gc).await);
}
