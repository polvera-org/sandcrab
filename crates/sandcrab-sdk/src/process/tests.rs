use std::sync::atomic::Ordering;
use std::time::Instant;

use sandcrab_core::SandboxConfig;
use sandcrab_core::testing::{FakeBackend, MockCrabd};

use super::*;
use crate::test_support::Env;
use crate::{Sandbox, Sandcrab};

const MIB: usize = 1024 * 1024;

struct Rig {
    _env: Env,
    _sandcrab: Sandcrab,
    backend: FakeBackend,
    sandbox: Sandbox,
    mock: MockCrabd,
}

async fn rig() -> Rig {
    let env = Env::new();
    let backend = FakeBackend::new();
    let sandcrab = env.sandcrab(&backend);
    let sandbox = sandcrab
        .spawn(SandboxConfig::from_template("base"))
        .await
        .unwrap();
    let mock = backend.mock_of(sandbox.id()).unwrap();
    Rig {
        _env: env,
        _sandcrab: sandcrab,
        backend,
        sandbox,
        mock,
    }
}

/// Poll `cond` for up to 5 s.
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn collect(output: &mut OutputStream) -> Vec<Output> {
    let mut all = Vec::new();
    while let Some(chunk) = output.next().await {
        all.push(chunk);
    }
    all
}

#[tokio::test]
async fn bytes_in_bytes_out_exit() {
    let r = rig().await;
    let (p, mut out) = r
        .sandbox
        .spawn(ProcessSpec::new(["echo"]).stdin(StdinMode::Pipe))
        .unwrap();
    // Written before the program started: kept and sent in order.
    p.write("hello").await.unwrap();
    assert!(p.started().await);
    assert_eq!(out.next().await, Some(Output::Stdout(b"hello".to_vec())));
    p.write(b" world".to_vec()).await.unwrap();
    assert_eq!(out.next().await, Some(Output::Stdout(b" world".to_vec())));
    assert!(p.try_exit().is_none());
    p.close_stdin().await.unwrap();
    p.close_stdin().await.unwrap();
    assert_eq!(out.next().await, None);
    let end = p.exit().await;
    assert!(matches!(end, ProcessOutcome::Exited(_)), "{end:?}");
    assert!(end.success() && end.tree_clean());
    assert_eq!(end.code(), Some(0));
    // Again, and after the end: the same answer, and writes are dropped without error.
    assert!(p.exit().await.success());
    p.write("late").await.unwrap();
    p.close_stdin().await.unwrap();
    assert_eq!(r.mock.processes.live(), 0);
}

#[tokio::test]
async fn exit_code_stderr_tail_env_and_stderr_stream() {
    let r = rig().await;
    let (p, mut out) = r.sandbox.spawn(ProcessSpec::new(["exit", "3"])).unwrap();
    assert_eq!(out.next().await, None);
    let end = p.exit().await;
    assert!(matches!(end, ProcessOutcome::Exited(_)), "{end:?}");
    assert_eq!((end.code(), end.stderr_tail()), (Some(3), "bye"));
    assert!(!end.success() && end.tree_clean());

    // The exact environment, nothing added.
    let (p, mut out) = r
        .sandbox
        .spawn(ProcessSpec::new(["env"]).env("B", "2").env("A", "1"))
        .unwrap();
    assert_eq!(
        collect(&mut out).await,
        [Output::Stdout(b"A=1\nB=2\n".to_vec())]
    );
    assert!(p.exit().await.success());

    // stderr as its own chunks, in the guest's order; the tail is kept too.
    let (p, mut out) = r
        .sandbox
        .spawn(ProcessSpec::new(["mixed"]).stderr(StderrMode::Stream))
        .unwrap();
    assert_eq!(
        collect(&mut out).await,
        [
            Output::Stdout(b"a".to_vec()),
            Output::Stderr(b"b".to_vec()),
            Output::Stdout(b"c".to_vec())
        ]
    );
    assert_eq!(p.exit().await.stderr_tail(), "b");
}

#[tokio::test]
async fn could_not_start_and_rejected() {
    let r = rig().await;
    for (argv, target) in [
        ("no-such-program", SpawnTarget::Program),
        ("no-cwd", SpawnTarget::Cwd),
    ] {
        let (p, mut out) = r
            .sandbox
            .spawn(ProcessSpec::new([argv]).stdin(StdinMode::Pipe))
            .unwrap();
        assert!(!p.started().await);
        match p.exit().await {
            ProcessOutcome::SpawnFailed { errno, what } => {
                assert_eq!((errno.as_str(), what), ("ENOENT", target));
            }
            other => panic!("{other:?}"),
        }
        assert!(p.exit().await.tree_clean());
        assert_eq!(out.next().await, None);
        // Nothing to write to and nothing to cancel: neither fails.
        p.write("x").await.unwrap();
        p.close_stdin().await.unwrap();
        let c = p.cancel(None).await;
        assert!(!c.was_running && c.tree_clean);
    }

    let (p, _out) = r.sandbox.spawn(ProcessSpec::new(["limit"])).unwrap();
    let end = p.exit().await;
    assert!(
        matches!(
            end,
            ProcessOutcome::Rejected(SandboxError::Protocol(ProtocolError::LimitExceeded { .. }))
        ),
        "{end:?}"
    );
    assert!(end.tree_clean() && end.exit_record().is_none());
    assert_eq!(r.mock.processes.opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_spec_fails_before_anything_starts() {
    let r = rig().await;
    for bad in [
        ProcessSpec::new(Vec::<String>::new()),
        ProcessSpec::new(["echo"]).cwd("relative"),
        ProcessSpec::new(["echo"]).env("A=B", "x"),
        ProcessSpec::new(["echo"]).timeout(Duration::ZERO),
        ProcessSpec::new(["echo"]).output_cap(0, Overflow::Kill),
    ] {
        let err = r.sandbox.spawn(bad.clone()).unwrap_err();
        assert!(
            matches!(err, SandboxError::Protocol(ProtocolError::InvalidParams(_))),
            "{err}"
        );
        assert!(r.sandbox.exec(bad).await.is_err());
    }
    assert_eq!(r.mock.processes.open_requests.load(Ordering::SeqCst), 0);
    // Environment values never reach a debug print.
    let spec = ProcessSpec::new(["echo"]).env("TOKEN", "s3cret");
    let shown = format!("{spec:?}");
    assert!(
        shown.contains("TOKEN") && !shown.contains("s3cret"),
        "{shown}"
    );
}

#[tokio::test]
async fn stdin_misuse_is_an_error() {
    let r = rig().await;
    let (p, _out) = r.sandbox.spawn(ProcessSpec::new(["sleep"])).unwrap();
    assert!(matches!(
        p.write("x").await,
        Err(SandboxError::InvalidConfig(_))
    ));
    p.close_stdin().await.unwrap();

    let (p, _out) = r
        .sandbox
        .spawn(ProcessSpec::new(["sleep"]).stdin(StdinMode::Pipe))
        .unwrap();
    p.write("x").await.unwrap();
    p.close_stdin().await.unwrap();
    assert!(matches!(
        p.write("y").await,
        Err(SandboxError::InvalidConfig(_))
    ));
}

#[tokio::test]
async fn cancel_stops_a_running_program_and_is_idempotent() {
    let r = rig().await;
    let (p, mut out) = r.sandbox.spawn(ProcessSpec::new(["sleep"])).unwrap();
    assert!(p.started().await);
    let first = p.cancel(Some(Duration::ZERO)).await;
    assert!(first.was_running && first.tree_clean);
    let end = p.exit().await;
    match &end {
        ProcessOutcome::Cancelled(Some(exit)) => {
            assert!(exit.cancelled);
            assert_eq!((exit.code, exit.signal), (None, Some(15)));
        }
        other => panic!("{other:?}"),
    }
    assert!(end.tree_clean() && !end.success());
    assert_eq!(out.next().await, None);
    // Later calls ask nobody.
    let again = p.cancel(None).await;
    assert!(again.tree_clean);
    assert_eq!(r.mock.processes.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(r.mock.processes.live(), 0);
}

#[tokio::test]
async fn concurrent_cancels_share_one_answer() {
    let r = rig().await;
    let (p, _out) = r.sandbox.spawn(ProcessSpec::new(["sleep"])).unwrap();
    assert!(p.started().await);
    let (a, b) = tokio::join!(p.cancel(None), p.cancel(Some(Duration::ZERO)));
    assert_eq!(a, b);
    assert!(a.was_running && a.tree_clean);
    assert_eq!(r.mock.processes.cancels.load(Ordering::SeqCst), 1);
    assert!(matches!(p.exit().await, ProcessOutcome::Cancelled(Some(_))));
}

#[tokio::test]
async fn cancel_before_the_start_prevents_it() {
    let r = rig().await;
    // Cancel at once: the driver has not run yet.
    let (p, mut out) = r
        .sandbox
        .spawn(ProcessSpec::new(["slow-start", "100"]))
        .unwrap();
    let c = p.cancel(None).await;
    assert!(!c.was_running && c.tree_clean);
    let end = p.exit().await;
    assert!(matches!(end, ProcessOutcome::Cancelled(None)), "{end:?}");
    assert!(end.tree_clean() && !p.started().await);
    assert_eq!(out.next().await, None);
    assert_eq!(r.mock.processes.opened.load(Ordering::SeqCst), 0);

    // Cancel while the guest is still starting the program.
    let before = r.mock.processes.open_requests.load(Ordering::SeqCst);
    let (p, _out) = r
        .sandbox
        .spawn(ProcessSpec::new(["slow-start", "200"]))
        .unwrap();
    eventually("the open reached the guest", || {
        r.mock.processes.open_requests.load(Ordering::SeqCst) > before
    })
    .await;
    let c = p.cancel(None).await;
    assert!(!c.was_running && c.tree_clean);
    let end = p.exit().await;
    assert!(matches!(end, ProcessOutcome::Cancelled(None)), "{end:?}");
    assert_eq!(r.mock.processes.opened.load(Ordering::SeqCst), 0);
    assert_eq!(r.mock.processes.live(), 0);
}

#[tokio::test]
async fn timed_out_and_overflow() {
    let r = rig().await;
    let (p, _out) = r
        .sandbox
        .spawn(ProcessSpec::new(["sleep"]).timeout(Duration::from_millis(50)))
        .unwrap();
    let end = p.exit().await;
    assert!(matches!(end, ProcessOutcome::TimedOut(_)), "{end:?}");
    assert!(end.tree_clean() && !end.success());
    assert_eq!(end.exit_record().unwrap().signal, Some(15));

    let (p, mut out) = r
        .sandbox
        .spawn(ProcessSpec::new(["emit", "100000"]).output_cap(70_000, Overflow::Kill))
        .unwrap();
    let got: usize = collect(&mut out)
        .await
        .iter()
        .map(|c| match c {
            Output::Stdout(d) | Output::Stderr(d) => d.len(),
        })
        .sum();
    assert_eq!(got, 70_000);
    let end = p.exit().await;
    assert!(matches!(end, ProcessOutcome::Overflow(_)), "{end:?}");

    // A cancel wins over a timeout, a timeout over the cap.
    let exit = |timed_out, cancelled, overflow| ProcessExit {
        code: None,
        signal: Some(9),
        stderr_tail: String::new(),
        timed_out,
        cancelled,
        overflow,
        tree_clean: false,
        duration_ms: 1,
    };
    let all = ProcessOutcome::from_exit(exit(true, true, true));
    assert!(matches!(all, ProcessOutcome::Cancelled(Some(_))));
    assert!(!all.tree_clean());
    assert!(matches!(
        ProcessOutcome::from_exit(exit(true, false, true)),
        ProcessOutcome::TimedOut(_)
    ));
}

#[tokio::test]
async fn unavailable_when_nothing_could_be_sent() {
    let r = rig().await;
    // The next connect fails: the request is never written.
    r.backend.connect_failures.store(1, Ordering::SeqCst);
    let (p, mut out) = r
        .sandbox
        .spawn(ProcessSpec::new(["echo"]).stdin(StdinMode::Pipe))
        .unwrap();
    let end = p.exit().await;
    assert!(matches!(end, ProcessOutcome::Unavailable(_)), "{end:?}");
    assert!(end.tree_clean() && !p.started().await);
    assert_eq!(out.next().await, None);
    p.write("x").await.unwrap();
    assert_eq!(r.mock.processes.open_requests.load(Ordering::SeqCst), 0);

    // A destroyed sandbox: the same, and a cancel has nothing left to stop.
    r.sandbox.core().destroy().await.unwrap();
    let (p, _out) = r.sandbox.spawn(ProcessSpec::new(["echo"])).unwrap();
    let end = p.exit().await;
    assert!(matches!(end, ProcessOutcome::Unavailable(_)), "{end:?}");
    assert!(p.cancel(None).await.tree_clean);
}

#[tokio::test]
async fn lost_when_the_connection_ends_without_an_exit() {
    let r = rig().await;
    let (p, mut out) = r.sandbox.spawn(ProcessSpec::new(["hangup"])).unwrap();
    assert!(p.started().await);
    let end = p.exit().await;
    assert!(matches!(end, ProcessOutcome::Lost(_)), "{end:?}");
    // Unknown is not clean.
    assert!(!end.tree_clean() && end.exit_record().is_none());
    assert_eq!(out.next().await, None);

    // The request was written, the answer never came: it may have run.
    let (p, _out) = r
        .sandbox
        .spawn(ProcessSpec::new(["hangup-before-ack"]))
        .unwrap();
    let end = p.exit().await;
    assert!(matches!(end, ProcessOutcome::Lost(_)), "{end:?}");
    assert!(!p.started().await);
}

#[tokio::test]
async fn unread_output_holds_the_program_back() {
    let r = rig().await;
    let n = 4 * MIB;
    let (p, mut out) = r
        .sandbox
        .spawn(ProcessSpec::new(["emit".to_owned(), n.to_string()]))
        .unwrap();
    assert!(p.started().await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Nobody reads: a few records are in flight, the rest is not written yet.
    let written = r.mock.processes.stdout_written.load(Ordering::SeqCst);
    assert!(written <= 512 * 1024, "{written} bytes written unread");
    assert!(p.try_exit().is_none());

    let mut got = Vec::with_capacity(n);
    while let Some(chunk) = out.next().await {
        match chunk {
            Output::Stdout(d) => got.extend_from_slice(&d),
            Output::Stderr(_) => panic!("no stderr expected"),
        }
    }
    assert_eq!(got.len(), n);
    assert!(got.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
    assert!(p.exit().await.success());
}

#[tokio::test]
async fn a_dropped_stream_and_a_cancel_discard_unread_output() {
    let r = rig().await;
    let n = 4 * MIB;
    let (p, out) = r
        .sandbox
        .spawn(ProcessSpec::new(["emit".to_owned(), n.to_string()]))
        .unwrap();
    drop(out);
    assert!(p.exit().await.success());
    assert_eq!(
        r.mock.processes.stdout_written.load(Ordering::SeqCst),
        n as u64
    );

    // Held but never read. Three records: the program has ended (the last record and the
    // exit fit in the transport), but the exit is behind output nobody takes.
    let (p, mut out) = r
        .sandbox
        .spawn(ProcessSpec::new(["emit", "150000"]))
        .unwrap();
    eventually("the program ended", || r.mock.processes.live() == 0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(p.try_exit().is_none());
    // A cancel discards it, so the exit is reached.
    let c = p.cancel(Some(Duration::ZERO)).await;
    assert!(!c.was_running && c.tree_clean);
    let end = tokio::time::timeout(Duration::from_secs(5), p.exit())
        .await
        .expect("exit after cancel");
    assert!(end.success(), "{end:?}");
    // What was already handed over, then the end.
    assert_eq!(collect(&mut out).await.len(), 1);
}

#[tokio::test]
async fn dropping_the_handle_stops_the_program() {
    let r = rig().await;
    // Running.
    let (p, out) = r.sandbox.spawn(ProcessSpec::new(["sleep"])).unwrap();
    assert!(p.started().await);
    assert_eq!(r.mock.processes.live(), 1);
    drop(p);
    eventually("the program stopped", || r.mock.processes.live() == 0).await;
    assert_eq!(r.mock.processes.closed_by_host.load(Ordering::SeqCst), 1);
    assert_eq!(r.mock.processes.cancels.load(Ordering::SeqCst), 0);
    // The stream of a dropped handle ends.
    let mut out = out;
    assert_eq!(out.next().await, None);

    // Still starting in the guest.
    let (p, _out) = r
        .sandbox
        .spawn(ProcessSpec::new(["slow-start", "100"]))
        .unwrap();
    eventually("the open reached the guest", || {
        r.mock.processes.open_requests.load(Ordering::SeqCst) == 2
    })
    .await;
    drop(p);
    eventually("the program stopped", || r.mock.processes.live() == 0).await;
    assert_eq!(r.mock.processes.closed_by_host.load(Ordering::SeqCst), 2);

    // Blocked on output nobody reads.
    let (p, out) = r
        .sandbox
        .spawn(ProcessSpec::new(["emit".to_owned(), (4 * MIB).to_string()]))
        .unwrap();
    assert!(p.started().await);
    drop((p, out));
    eventually("the program stopped", || r.mock.processes.live() == 0).await;

    // The sandbox itself is untouched and still runs programs.
    assert!(!r.sandbox.is_destroyed());
    let out = r
        .sandbox
        .exec(ProcessSpec::new(["exit", "0"]))
        .await
        .unwrap();
    assert!(out.outcome.success());
}

#[tokio::test]
async fn stdin_backpressure_and_a_dropped_write_stay_whole() {
    let r = rig().await;
    let (p, mut out) = r
        .sandbox
        .spawn(ProcessSpec::new(["echo"]).stdin(StdinMode::Pipe))
        .unwrap();
    assert!(p.started().await);
    // Nobody reads the echo: a large write cannot finish.
    let big = vec![7u8; 2 * MIB];
    let write = p.write(big.clone());
    assert!(
        tokio::time::timeout(Duration::from_millis(200), write)
            .await
            .is_err()
    );
    // The abandoned write is still sent whole once the output is read: no torn record.
    let reader = async {
        let mut got = Vec::new();
        while let Some(Output::Stdout(d)) = out.next().await {
            got.extend_from_slice(&d);
        }
        got
    };
    let closer = async {
        p.write("end").await.unwrap();
        p.close_stdin().await.unwrap();
        p.exit().await
    };
    let (got, end) = tokio::join!(reader, closer);
    assert!(end.success(), "{end:?}");
    assert_eq!(got.len(), big.len() + 3);
    assert!(got[..big.len()].iter().all(|b| *b == 7));
    assert_eq!(&got[big.len()..], b"end");
}

#[tokio::test]
async fn exec_collects_bounded_output() {
    let r = rig().await;
    let out = r
        .sandbox
        .exec(ProcessSpec::new(["emit", "1000"]))
        .await
        .unwrap();
    assert!(out.outcome.success());
    assert_eq!(out.stdout.len(), 1000);
    assert!(out.stderr.is_empty() && !out.stderr_truncated);

    // A program that waits for stdin sees its end at once.
    let out = r
        .sandbox
        .exec(ProcessSpec::new(["echo"]).stdin(StdinMode::Pipe))
        .await
        .unwrap();
    assert!(out.outcome.success() && out.stdout.is_empty());

    // The default cap.
    let out = r
        .sandbox
        .exec(ProcessSpec::new(["emit".to_owned(), (3 * MIB).to_string()]))
        .await
        .unwrap();
    assert!(
        matches!(out.outcome, ProcessOutcome::Overflow(_)),
        "{:?}",
        out.outcome
    );
    assert_eq!(out.stdout.len() as u64, DEFAULT_EXEC_OUTPUT_CAP);

    // The spec's own timeout.
    let out = r
        .sandbox
        .exec(ProcessSpec::new(["sleep"]).timeout(Duration::from_millis(50)))
        .await
        .unwrap();
    assert!(
        matches!(out.outcome, ProcessOutcome::TimedOut(_)),
        "{:?}",
        out.outcome
    );

    // Streamed stderr is collected next to stdout; failures are outcomes.
    let out = r
        .sandbox
        .exec(ProcessSpec::new(["mixed"]).stderr(StderrMode::Stream))
        .await
        .unwrap();
    assert_eq!(
        (out.stdout.as_slice(), out.stderr.as_slice()),
        (&b"ac"[..], &b"b"[..])
    );
    let out = r.sandbox.exec(ProcessSpec::new(["nope"])).await.unwrap();
    assert!(
        matches!(out.outcome, ProcessOutcome::SpawnFailed { .. }),
        "{:?}",
        out.outcome
    );
    assert_eq!(r.mock.processes.live(), 0);
}

#[tokio::test]
async fn dropping_exec_stops_the_program() {
    let r = rig().await;
    let exec = r.sandbox.exec(ProcessSpec::new(["sleep"]));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), exec)
            .await
            .is_err()
    );
    eventually("the program stopped", || r.mock.processes.live() == 0).await;
    assert_eq!(r.mock.processes.opened.load(Ordering::SeqCst), 1);
    assert_eq!(r.mock.processes.closed_by_host.load(Ordering::SeqCst), 1);
}
