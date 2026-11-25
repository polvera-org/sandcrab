//! The core of PID 1's zombie reaper: collect every exited child, whatever its process group.
//!
//! Every orphan in the guest is re-parented to `crabd init` (PID 1; nothing sets a child
//! subreaper). Tool processes run in their own process group (`process_group(0)`) or session
//! (PTY `setsid`), so their orphans are never in PID 1's group. The reaper must therefore
//! wait on **any** child (`waitpid(-1)`). `rustix::process::waitpid(None, ..)` is
//! `waitpid(0)`: only children in the caller's process group. That missed every orphan
//! and left them as zombies.
//!
//! Why a global `waitpid(-1)` is safe here: PID 1 never waits on a specific child.
//! - Its only spawned child is `crabd serve`. The `Child` is dropped without `wait`, and the
//!   reaper reports that pid's exit.
//! - `std`'s own `waitpid` on an exec failure inside `Command::spawn` is protected by the
//!   caller holding a lock across `spawn` and `reap_exited`.
//! - Tool processes (`bash`, `process_start`, PTY sessions) are children of `crabd serve`, a
//!   different process. They are reaped by tokio's `Child::wait` there, so PID 1 can never
//!   steal their exit statuses. Only once their parent is gone (an orphan) do they become
//!   PID 1's children.
//!
//! Any future code in PID 1 that waits on a specific child must hand its pid to the reaper
//! instead, or a concurrent `waitpid(-1)` will collect it first.

use rustix::{
    io::Errno,
    process::{WaitOptions, WaitStatus},
};
use tracing::warn;

/// Reap every child that has already exited, without blocking: `waitpid(-1, WNOHANG)` until
/// none is left. Calls `on_exit(pid, status)` for each child reaped and returns how many.
pub fn reap_exited(mut on_exit: impl FnMut(u32, WaitStatus)) -> usize {
    let mut n = 0;
    loop {
        match rustix::process::wait(WaitOptions::NOHANG) {
            Ok(Some((pid, status))) => {
                n += 1;
                on_exit(pid.as_raw_nonzero().get().unsigned_abs(), status);
            }
            Ok(None) | Err(Errno::CHILD) => break,
            Err(Errno::INTR) => {}
            Err(e) => {
                warn!(error = %e, "waitpid(-1)");
                break;
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Read,
        os::unix::process::CommandExt,
        process::Command,
        time::{Duration, Instant},
    };

    /// Set in the re-executed test binary that runs the body in isolation.
    const CHILD_ENV: &str = "SANDCRAB_REAP_TEST_CHILD";
    const NAME: &str = "init::reap::tests::reaps_children_outside_our_process_group";

    /// `waitpid(-1)` collects children of *any* process group, which is how orphans of tool
    /// processes look to PID 1. The body runs in a fresh copy of this test binary, so the
    /// global wait cannot steal exit statuses from other tests running in parallel.
    #[test]
    fn reaps_children_outside_our_process_group() {
        if std::env::var_os(CHILD_ENV).is_some() {
            isolated_body();
            return;
        }
        let out = Command::new(std::env::current_exe().unwrap())
            .args([NAME, "--exact", "--test-threads=1", "--nocapture"])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(stdout.contains("1 passed"), "stdout:\n{stdout}");
    }

    fn isolated_body() {
        // Like PID 1: adopt orphans (Linux only), so an orphaned grandchild is ours to reap.
        #[cfg(target_os = "linux")]
        rustix::process::set_child_subreaper(Some(rustix::process::getpid())).unwrap();

        // A child leading its own process group, as the bash tool spawns them. On Linux it
        // also leaves an orphan (in that group) that exits shortly after it.
        let mut child = Command::new("sh")
            .args(["-c", "sleep 0.2 & echo $!; exit 7"])
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        // EOF once `sh` and the backgrounded `sleep` (which inherits the pipe) have exited.
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        // Dropped without `wait`: only the reaper may collect it.
        drop(child);
        let orphan: u32 = out.trim().parse().unwrap();

        let mut want = vec![(pid, Some(7))];
        if cfg!(target_os = "linux") {
            want.push((orphan, Some(0)));
        }
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while got.len() < want.len() && Instant::now() < deadline {
            reap_exited(|p, s| got.push((p, s.exit_status())));
            std::thread::sleep(Duration::from_millis(20));
        }
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want, "reaped (pid, exit status)");
    }
}
