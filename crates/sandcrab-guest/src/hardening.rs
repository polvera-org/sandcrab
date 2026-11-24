//! Process hardening that `crabd serve` applies to itself, whatever started it.
//!
//! Docker sets `no-new-privileges` on the container. The microVM backends have no such
//! switch: `crabd init` (root, PID 1) starts `crabd serve` as the sandbox user, and every
//! tool process (`bash`, `process_start`, PTY sessions) is a child of `serve`. So `serve`
//! sets `no_new_privs` itself, before it starts any thread or process. That covers every
//! process the agent can run, on every backend. Setting it on PID 1 is not needed: PID 1
//! only ever execs `crabd serve`, and keeps its privileges for mounts, export and reboot.
//!
//! `no_new_privs` is a per-thread attribute, inherited by every thread and process created
//! afterwards and never cleared. That is why it is set on the main thread before the tokio
//! runtime starts its workers, rather than in a `pre_exec` hook (which would need `unsafe`).

use std::io;

/// Set `no_new_privs` on the calling thread: from now on, `execve` can never grant more
/// privileges (setuid / setgid bits and file capabilities are ignored). Call it before
/// spawning any thread so every later thread and child inherits it. A no-op off Linux.
pub fn forbid_privilege_gain() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        rustix::thread::set_no_new_privs(true)?;
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn children_inherit_no_new_privs() {
        // A fresh thread, so the test runner's other threads are unaffected.
        let status = std::thread::spawn(|| {
            forbid_privilege_gain().unwrap();
            assert!(rustix::thread::no_new_privs().unwrap());
            std::process::Command::new("cat")
                .arg("/proc/self/status")
                .output()
                .unwrap()
        })
        .join()
        .unwrap();
        let status = String::from_utf8(status.stdout).unwrap();
        assert!(
            status.lines().any(|l| l == "NoNewPrivs:\t1"),
            "child status:\n{status}"
        );
    }
}
