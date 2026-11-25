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
//!
//! The capability **bounding set** is the other half of Docker's `cap_drop: ALL` (which
//! leaves `CapBnd` = 0 in the container). `crabd serve` cannot clear it itself: it starts
//! as the unprivileged sandbox user, and `PR_CAPBSET_DROP` needs `CAP_SETPCAP`. So PID 1
//! clears it with [`clear_capability_bounding_set`] on a short-lived thread that then forks
//! `crabd serve`. The bounding set is per thread and copied into the child, so `serve` and
//! every tool process get an empty one, while PID 1's other threads keep theirs (PID 1
//! never execs anything else, so its own bounding set grants nothing).

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

/// Empty the calling thread's capability bounding set, then check that
/// `/proc/thread-self/status` reports `CapBnd: 0` (fail closed: a capability the kernel knows
/// but that could not be dropped is an error). Needs `CAP_SETPCAP`; children forked from
/// this thread afterwards inherit the empty set. Errors off Linux.
pub fn clear_capability_bounding_set() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use rustix::{
            io::Errno,
            thread::{CapabilitySet, remove_capability_from_bounding_set},
        };
        // Every capability number up to the kernel's `CAP_LAST_CAP`, including ones newer
        // than rustix's named constants; the first unknown number fails with `EINVAL`.
        for cap in 0..u64::BITS {
            match remove_capability_from_bounding_set(CapabilitySet::from_bits_retain(1 << cap)) {
                Ok(()) => {}
                Err(Errno::INVAL) if cap > 0 => break,
                Err(e) => return Err(e.into()),
            }
        }
        let status = std::fs::read_to_string("/proc/thread-self/status")?;
        match cap_mask(&status, "CapBnd") {
            Some(0) => Ok(()),
            other => Err(io::Error::other(format!(
                "capability bounding set not empty after dropping: {other:?}"
            ))),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "capability bounding sets are Linux-only",
        ))
    }
}

/// A hex capability mask (`CapBnd`, `CapEff`, ...) from a `/proc/<pid>/status` text.
#[must_use]
pub fn cap_mask(status: &str, field: &str) -> Option<u64> {
    status.lines().find_map(|l| {
        let v = l.strip_prefix(field)?.strip_prefix(':')?;
        u64::from_str_radix(v.trim(), 16).ok()
    })
}

#[cfg(test)]
mod portable_tests {
    use super::*;

    #[test]
    fn parses_capability_masks() {
        let status = "Name:\tcrabd\nCapInh:\t0000000000000000\nCapBnd:\t000001ffffffffff\nCapAmb:\t0000000000000000\n";
        assert_eq!(cap_mask(status, "CapBnd"), Some(0x1ff_ffff_ffff));
        assert_eq!(cap_mask(status, "CapInh"), Some(0));
        assert_eq!(cap_mask(status, "CapEff"), None);
        assert_eq!(cap_mask("CapBndX:\t0\n", "CapBnd"), None);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn status_of(cmd: &str) -> String {
        let out = std::process::Command::new(cmd)
            .arg("/proc/self/status")
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn bounding_set_cleared_for_children_or_refused_unprivileged() {
        let before = cap_mask(&status_of("cat"), "CapBnd");
        // A fresh thread: the bounding set is per thread, so the runner's threads keep theirs.
        let (res, child) = std::thread::spawn(|| {
            let res = clear_capability_bounding_set();
            (res, status_of("cat"))
        })
        .join()
        .unwrap();
        if rustix::process::geteuid().is_root() {
            res.unwrap();
            assert_eq!(cap_mask(&child, "CapBnd"), Some(0), "child:\n{child}");
        } else {
            // Without CAP_SETPCAP the drop fails loudly (EPERM), never silently.
            assert_eq!(
                res.unwrap_err().raw_os_error(),
                Some(rustix::io::Errno::PERM.raw_os_error())
            );
        }
        // Only that thread changed.
        assert_eq!(cap_mask(&status_of("cat"), "CapBnd"), before);
    }

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
