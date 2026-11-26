//! The process tree of a `crabd supervise`: every process whose parent chain leads to it.
//!
//! The supervisor is a child subreaper, so a descendant whose parent died is re-parented to
//! the supervisor instead of PID 1. A grandchild that calls `setsid`, leaves the process group
//! or double-forks therefore stays in the tree: no unprivileged process can change who its
//! ancestors are. The tree is read from `/proc/<pid>/stat` (`pid`, `state`, `ppid`).
//!
//! The listing is a snapshot: a process may fork or exit while it is read. The supervisor's
//! force loop lists again until two listings in a row are empty (see `crate::supervise`).

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};

/// One process from `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: u32,
    pub ppid: u32,
    /// The state letter (`R`, `S`, `D`, `Z`, `X`, `T`, ...).
    pub state: char,
}

impl ProcStat {
    /// A zombie (or dead) process: it has exited and only waits to be reaped.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        matches!(self.state, 'Z' | 'X' | 'x')
    }
}

/// Parse the content of a `/proc/<pid>/stat` file. The command name is in parentheses and may
/// itself contain spaces and `)`, so the fields after it are found from the **last** `)`.
#[must_use]
pub fn parse_stat(text: &str) -> Option<ProcStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let pid = text[..open].trim().parse().ok()?;
    let mut rest = text.get(close + 1..)?.split_ascii_whitespace();
    let state = rest.next()?.chars().next()?;
    let ppid = rest.next()?.parse().ok()?;
    Some(ProcStat { pid, ppid, state })
}

/// Reads process trees from a `/proc` directory (a fixture directory in tests).
#[derive(Debug, Clone)]
pub struct ProcFs {
    root: PathBuf,
}

impl Default for ProcFs {
    fn default() -> Self {
        Self::new("/proc")
    }
}

impl ProcFs {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Every process visible in this `/proc`. Entries that vanish while they are read are
    /// skipped (the process exited).
    pub fn all(&self) -> io::Result<Vec<ProcStat>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            match std::fs::read_to_string(stat_path(&self.root, name)) {
                Ok(text) => out.extend(parse_stat(&text)),
                Err(e) if is_gone(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// The descendants of `ancestor` (not `ancestor` itself), dead ones included.
    pub fn descendants(&self, ancestor: u32) -> io::Result<Vec<ProcStat>> {
        Ok(descendants_of(&self.all()?, ancestor))
    }
}

fn stat_path(root: &Path, pid: &str) -> PathBuf {
    root.join(pid).join("stat")
}

/// `ENOENT` or `ESRCH`: the process exited between `readdir` and `open`/`read`.
fn is_gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(3)
}

/// The processes of `all` whose parent chain reaches `ancestor`, in `all`'s order.
#[must_use]
pub fn descendants_of(all: &[ProcStat], ancestor: u32) -> Vec<ProcStat> {
    let parent: HashMap<u32, u32> = all.iter().map(|p| (p.pid, p.ppid)).collect();
    all.iter()
        .filter(|p| {
            let mut cur = p.pid;
            // A chain is at most as long as the table; the bound also stops a cycle that a
            // torn snapshot could show (a pid reused between two reads).
            for _ in 0..=parent.len() {
                if cur == ancestor {
                    return p.pid != ancestor;
                }
                match parent.get(&cur) {
                    Some(&pp) if pp != 0 && pp != cur => cur = pp,
                    _ => return false,
                }
            }
            false
        })
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_odd_command_names() {
        let p = parse_stat("42 (my prog) S 7 42 42 0 -1 4194560 100 0").unwrap();
        assert_eq!(
            p,
            ProcStat {
                pid: 42,
                ppid: 7,
                state: 'S'
            }
        );
        // `)` and spaces inside the name.
        let p = parse_stat("9 (a) b) (c) Z 1 9 9 0").unwrap();
        assert_eq!((p.pid, p.ppid, p.state), (9, 1, 'Z'));
        assert!(p.is_dead());
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x)"), None);
    }

    fn st(pid: u32, ppid: u32) -> ProcStat {
        ProcStat {
            pid,
            ppid,
            state: 'S',
        }
    }

    #[test]
    fn descendants_follow_the_parent_chain() {
        // 100 = supervisor; 101 leader; 102 child; 103 grandchild re-parented to 100 after
        // its parent died (setsid escapee); 200 unrelated; 300 a cycle from a torn read.
        let all = [
            st(1, 0),
            st(100, 1),
            st(101, 100),
            st(102, 101),
            st(103, 100),
            st(104, 103),
            st(200, 1),
            st(201, 200),
            st(300, 301),
            st(301, 300),
        ];
        let mut d: Vec<u32> = descendants_of(&all, 100).iter().map(|p| p.pid).collect();
        d.sort_unstable();
        assert_eq!(d, [101, 102, 103, 104]);
        assert!(descendants_of(&all, 999).is_empty());
        assert!(descendants_of(&all, 104).is_empty());
    }

    #[test]
    fn reads_a_fixture_proc() {
        let dir = tempfile::tempdir().unwrap();
        for (pid, line) in [
            ("1", "1 (init) S 0 1 1 0"),
            ("50", "50 (crabd) S 1 50 50 0"),
            ("51", "51 (sh -c x) S 50 51 51 0"),
            ("52", "52 (sleep) Z 51 51 51 0"),
            ("60", "60 (other) R 1 60 60 0"),
        ] {
            std::fs::create_dir(dir.path().join(pid)).unwrap();
            std::fs::write(dir.path().join(pid).join("stat"), line).unwrap();
        }
        // Not processes.
        std::fs::create_dir(dir.path().join("self")).unwrap();
        std::fs::write(dir.path().join("uptime"), "1 2").unwrap();
        // A pid directory whose stat vanished (the process exited mid-listing).
        std::fs::create_dir(dir.path().join("70")).unwrap();

        let fs = ProcFs::new(dir.path());
        assert_eq!(fs.all().unwrap().len(), 5);
        let mut d = fs.descendants(50).unwrap();
        d.sort_by_key(|p| p.pid);
        assert_eq!(d.iter().map(|p| p.pid).collect::<Vec<_>>(), [51, 52]);
        assert!(d[1].is_dead());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_proc_lists_a_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let me = std::process::id();
        let d = ProcFs::default().descendants(me).unwrap();
        assert!(d.iter().any(|p| p.pid == child.id()), "{d:?}");
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
