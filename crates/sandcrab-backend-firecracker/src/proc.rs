//! Process identity helpers: a pid plus its `/proc/<pid>/stat` start time identifies a
//! process without PID-reuse races (design §7).

use std::fs;

use rustix::process::{Pid, Signal, kill_process, kill_process_group, test_kill_process};
use serde::{Deserialize, Serialize};

/// A process identity that survives PID reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcId {
    pub pid: u32,
    /// Field 22 of `/proc/<pid>/stat` (clock ticks since boot); `None` where `/proc` is
    /// unavailable.
    pub starttime: Option<u64>,
}

impl ProcId {
    /// Identity of `pid` right now (`None` if it does not exist).
    #[must_use]
    pub fn of(pid: u32) -> Option<Self> {
        let p = Pid::from_raw(i32::try_from(pid).ok()?)?;
        match test_kill_process(p) {
            // EPERM still means "exists".
            Ok(()) | Err(rustix::io::Errno::PERM) => {}
            Err(_) => return None,
        }
        Some(Self {
            pid,
            starttime: starttime(pid),
        })
    }

    /// The current process.
    #[must_use]
    pub fn current() -> Self {
        let pid = std::process::id();
        Self {
            pid,
            starttime: starttime(pid),
        }
    }

    /// True if this exact process (same pid *and* start time) is still running. A zombie
    /// counts as dead.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        match Self::of(self.pid) {
            Some(now) => {
                now.starttime == self.starttime && state(self.pid).is_none_or(|s| s != 'Z')
            }
            None => false,
        }
    }

    /// SIGKILL this process (and, if `group`, its process group) — only if it is still the
    /// same process.
    pub fn kill(&self, group: bool) -> bool {
        if !self.is_alive() {
            return false;
        }
        let Some(pid) = i32::try_from(self.pid).ok().and_then(Pid::from_raw) else {
            return false;
        };
        let res = if group {
            kill_process_group(pid, Signal::KILL).or_else(|_| kill_process(pid, Signal::KILL))
        } else {
            kill_process(pid, Signal::KILL)
        };
        res.is_ok()
    }
}

/// Field after `comm` (which may contain spaces/parens): split after the last `)`.
fn stat_fields(pid: u32) -> Option<Vec<String>> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_owned).collect())
}

/// `/proc/<pid>/stat` field 22 (`starttime`).
#[must_use]
pub fn starttime(pid: u32) -> Option<u64> {
    parse_starttime(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

fn parse_starttime(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // rest starts at field 3 (state); starttime is field 22 → index 19.
    rest.split_whitespace().nth(19)?.parse().ok()
}

fn state(pid: u32) -> Option<char> {
    stat_fields(pid)?.first()?.chars().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_odd_comm() {
        let stat = "1234 (fire cracker) (x)) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 3 0 987654 1000 10";
        assert_eq!(parse_starttime(stat), Some(987_654));
        assert_eq!(parse_starttime("garbage"), None);
    }

    #[test]
    fn current_process_is_alive_and_identity_checked() {
        let me = ProcId::current();
        assert!(me.is_alive());
        if me.starttime.is_some() {
            let impostor = ProcId {
                pid: me.pid,
                starttime: me.starttime.map(|t| t + 1),
            };
            assert!(!impostor.is_alive(), "same pid, different start time");
            assert!(!impostor.kill(false), "never kills a reused pid");
        }
        assert!(ProcId::of(u32::MAX).is_none());
    }

    #[test]
    fn kills_a_child_group() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let id = ProcId::of(child.id()).unwrap();
        assert!(id.is_alive());
        assert!(id.kill(false));
        child.wait().unwrap();
        assert!(!id.is_alive());
    }
}
