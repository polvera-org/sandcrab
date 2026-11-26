//! Process handle e2e: a program runs through [`sandcrab_sdk::Process`] against a real
//! `crabd`: bytes in, bytes out, cancel (the whole tree), timeout, drop, exit. See
//! `tests/common/mod.rs` for how to run.

mod common;

use std::time::{Duration, Instant};

use common::{run, sh};
use sandcrab_sdk::{
    Output, ProcessOutcome, ProcessSpec, Sandbox, SandboxConfig, SpawnTarget, StdinMode,
};

/// Programs in the sandbox whose command line carries `marker` (a regex that does not match
/// itself).
async fn running(sb: &Sandbox, marker: &str) -> usize {
    sh(
        sb,
        &format!("grep -la '{marker}' /proc/[0-9]*/cmdline 2>/dev/null | wc -l"),
    )
    .await
    .trim()
    .parse()
    .unwrap()
}

async fn wait_gone(sb: &Sandbox, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while running(sb, marker).await > 0 {
        assert!(Instant::now() < deadline, "`{marker}` is still running");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn process_handle_bytes_in_bytes_out_cancel_exit() {
    run("process-handle", |h| async move {
        let sb =
            h.sc.spawn(SandboxConfig::from_template("base"))
                .await
                .unwrap();

        // Bytes in, bytes out, exit.
        let (p, mut out) = sb
            .spawn(ProcessSpec::new(["/bin/cat"]).stdin(StdinMode::Pipe))
            .unwrap();
        p.write("hello\n").await.unwrap();
        assert!(p.started().await);
        assert_eq!(out.next().await, Some(Output::Stdout(b"hello\n".to_vec())));
        p.write("again\n").await.unwrap();
        assert_eq!(out.next().await, Some(Output::Stdout(b"again\n".to_vec())));
        p.close_stdin().await.unwrap();
        assert_eq!(out.next().await, None);
        let end = p.exit().await;
        assert!(end.success() && end.tree_clean(), "{end:?}");

        // The exact environment and argv, no shell.
        let got = sb
            .exec(ProcessSpec::new(["/usr/bin/env"]).env("ONLY", "this one"))
            .await
            .unwrap();
        assert!(got.outcome.success(), "{:?}", got.outcome);
        assert_eq!(String::from_utf8_lossy(&got.stdout), "ONLY=this one\n");

        // Exit code and stderr tail.
        let got = sb
            .exec(ProcessSpec::new(["/bin/sh", "-c", "echo oops >&2; exit 3"]))
            .await
            .unwrap();
        assert!(
            matches!(got.outcome, ProcessOutcome::Exited(_)),
            "{:?}",
            got.outcome
        );
        assert_eq!(
            (got.outcome.code(), got.outcome.stderr_tail()),
            (Some(3), "oops\n")
        );

        // Could not start.
        let got = sb
            .exec(ProcessSpec::new(["/no/such/program"]))
            .await
            .unwrap();
        match got.outcome {
            ProcessOutcome::SpawnFailed { errno, what } => {
                assert_eq!((errno.as_str(), what), ("ENOENT", SpawnTarget::Program));
            }
            other => panic!("{other:?}"),
        }

        // Timeout, in the guest.
        let got = sb
            .exec(ProcessSpec::new(["/bin/sleep", "271828"]).timeout(Duration::from_millis(300)))
            .await
            .unwrap();
        assert!(
            matches!(got.outcome, ProcessOutcome::TimedOut(_)),
            "{:?}",
            got.outcome
        );
        assert!(got.outcome.tree_clean());

        // Cancel stops the whole tree, and answers when nothing is left.
        let (p, _out) = sb
            .spawn(ProcessSpec::new([
                "/bin/sh",
                "-c",
                "sleep 314159 & sleep 314159",
            ]))
            .unwrap();
        assert!(p.started().await);
        assert!(running(&sb, "31[4]159").await >= 2);
        let cancel = p.cancel(Some(Duration::from_millis(200))).await;
        assert!(cancel.was_running && cancel.tree_clean, "{cancel:?}");
        assert_eq!(running(&sb, "31[4]159").await, 0);
        let end = p.exit().await;
        assert!(matches!(end, ProcessOutcome::Cancelled(Some(_))), "{end:?}");
        assert!(end.tree_clean());

        // Dropping the handle leaves nothing either.
        let (p, out) = sb
            .spawn(ProcessSpec::new([
                "/bin/sh",
                "-c",
                "sleep 161803 & sleep 161803",
            ]))
            .unwrap();
        assert!(p.started().await);
        assert!(running(&sb, "16[1]803").await >= 2);
        drop((p, out));
        wait_gone(&sb, "16[1]803").await;

        sb.destroy().await.unwrap();
    })
    .await;
}
