//! Orphan run dir reaping (`meta.json`, owner pid + start time) (design §2).
//!
//! The lifeline makes an orphaned helper kill itself as soon as its owner dies, so the reaper
//! mostly removes leftovers: for every `<run_dir>/<sandbox id>/meta.json` with
//! `backend = "libkrun"` whose owner (pid + start time) is gone, it SIGKILLs the helper's
//! process group if that pid still has the recorded start time, removes the cgroup leaf
//! (Linux) and deletes the run dir. There are no workspace images to recover (live
//! virtio-fs). Directories without a parseable `meta.json` are only removed once older than
//! [`STALE_AFTER`] (a concurrent spawn writes it right after `mkdir`). Nothing outside
//! `run_dir` is ever deleted, and other backends' run dirs live elsewhere.
//!
//! Process identity is `(pid, start time)` so a reused pid is never signalled. The start time
//! comes from `/proc/<pid>/stat` on Linux and from `ps -o lstart=` on macOS (no `unsafe`
//! `sysctl`, design §2).

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::process::{Pid, Signal, kill_process, kill_process_group, test_kill_process};
use sandcrab_core::{Result, SandboxError, SandboxId};
use serde::{Deserialize, Serialize};

/// `meta.json` inside a run dir.
pub const META_FILE: &str = "meta.json";
/// `backend` value in `meta.json`.
pub const META_BACKEND: &str = "libkrun";
/// Age after which a run dir without metadata is considered abandoned.
pub const STALE_AFTER: Duration = Duration::from_secs(60);

/// A process identity that survives pid reuse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcId {
    pub pid: u32,
    /// Opaque start time: `/proc/<pid>/stat` field 22 on Linux, `ps -o lstart=` on macOS.
    /// `None` when it could not be read (then only the pid is compared).
    pub start: Option<String>,
}

impl ProcId {
    /// Identity of `pid` right now (`None` if no such process).
    #[must_use]
    pub fn of(pid: u32) -> Option<Self> {
        if !exists(pid) {
            return None;
        }
        Some(Self {
            pid,
            start: proc_info(pid).map(|i| i.start),
        })
    }

    /// The current process (cached: it never changes).
    #[must_use]
    pub fn current() -> Self {
        static ME: OnceLock<ProcId> = OnceLock::new();
        ME.get_or_init(|| {
            let pid = std::process::id();
            Self {
                pid,
                start: proc_info(pid).map(|i| i.start),
            }
        })
        .clone()
    }

    /// True if this exact process (same pid *and* start time) still runs. Zombies are dead.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        if !exists(self.pid) {
            return false;
        }
        match proc_info(self.pid) {
            Some(now) => self.start.as_ref().is_none_or(|s| *s == now.start) && !now.zombie,
            // Exists but unreadable: trust the pid only if we never had a start time.
            None => self.start.is_none(),
        }
    }

    /// Send `signal` to this process (its process group too if `group`), only if it is still
    /// the same process. Returns whether a signal was sent.
    pub fn signal(&self, signal: Signal, group: bool) -> bool {
        if !self.is_alive() {
            return false;
        }
        let Some(pid) = raw_pid(self.pid) else {
            return false;
        };
        let res = if group {
            kill_process_group(pid, signal).or_else(|_| kill_process(pid, signal))
        } else {
            kill_process(pid, signal)
        };
        res.is_ok()
    }
}

fn raw_pid(pid: u32) -> Option<Pid> {
    Pid::from_raw(i32::try_from(pid).ok()?)
}

fn exists(pid: u32) -> bool {
    raw_pid(pid).is_some_and(|p| {
        // EPERM still means "exists".
        matches!(test_kill_process(p), Ok(()) | Err(rustix::io::Errno::PERM))
    })
}

struct ProcInfo {
    start: String,
    zombie: bool,
}

#[cfg(target_os = "linux")]
fn proc_info(pid: u32) -> Option<ProcInfo> {
    parse_proc_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// `/proc/<pid>/stat`: fields after `comm` (which may contain spaces and parens).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_stat(stat: &str) -> Option<ProcInfo> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // fields[0] is field 3 (state); starttime is field 22 → index 19.
    Some(ProcInfo {
        zombie: fields.first()?.starts_with('Z'),
        start: (*fields.get(19)?).to_owned(),
    })
}

#[cfg(not(target_os = "linux"))]
fn proc_info(pid: u32) -> Option<ProcInfo> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=,lstart=", "-p", &pid.to_string()])
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ps_line(&String::from_utf8_lossy(&out.stdout))
}

/// `ps -o stat=,lstart=` line: `S    Tue Nov 24 10:00:00 2026`.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_ps_line(line: &str) -> Option<ProcInfo> {
    let line = line.trim();
    let (stat, start) = line.split_once(char::is_whitespace)?;
    let start = start.split_whitespace().collect::<Vec<_>>().join(" ");
    if start.is_empty() {
        return None;
    }
    Some(ProcInfo {
        zombie: stat.starts_with('Z'),
        start,
    })
}

/// `meta.json`: what an orphan reaper needs to clean up after a dead owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMeta {
    pub backend: String,
    pub sandbox_id: SandboxId,
    /// The sandcrab process that owns this sandbox.
    pub owner: ProcId,
    /// The `sandcrab-krun` helper (also its process group leader), once spawned.
    #[serde(default)]
    pub helper: Option<ProcId>,
    /// Linux cgroup v2 leaf of the helper.
    #[serde(default)]
    pub cgroup: Option<PathBuf>,
    #[serde(default)]
    pub guest_boot_ms: Option<u64>,
    pub created_unix_ms: u64,
}

impl RunMeta {
    #[must_use]
    pub fn new(id: SandboxId) -> Self {
        Self {
            backend: META_BACKEND.into(),
            sandbox_id: id,
            owner: ProcId::current(),
            helper: None,
            cgroup: None,
            guest_boot_ms: None,
            created_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        }
    }

    pub fn load(dir: &Path) -> io::Result<Self> {
        let data = fs::read(dir.join(META_FILE))?;
        serde_json::from_slice(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Atomic write (temp file + rename).
    pub fn save(&self, dir: &Path) -> Result<()> {
        let tmp = dir.join(".meta.json.tmp");
        let path = dir.join(META_FILE);
        let json = serde_json::to_vec_pretty(self).expect("meta serializes");
        let write = || -> io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&json)?;
            fs::rename(&tmp, &path)
        };
        write().map_err(|e| io_at(&path, e))
    }
}

/// Create `<run_dir>/<id>/` (0700, parents 0700) and write the initial `meta.json`. Never
/// reuses an existing directory.
pub fn create_run_dir(run_dir: &Path, id: SandboxId) -> Result<(PathBuf, RunMeta)> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(run_dir)
        .map_err(|e| io_at(run_dir, e))?;
    let dir = run_dir.join(id.to_string());
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| io_at(&dir, e))?;
    let meta = RunMeta::new(id);
    if let Err(e) = meta.save(&dir) {
        let _ = fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok((dir, meta))
}

pub(crate) fn io_at(path: &Path, e: io::Error) -> SandboxError {
    SandboxError::Io(io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

/// What [`reap_run_dir`] cleaned up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReapReport {
    /// Run dirs removed.
    pub run_dirs: Vec<PathBuf>,
    /// Helper pids killed.
    pub killed: Vec<u32>,
}

impl ReapReport {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.run_dirs.is_empty() && self.killed.is_empty()
    }
}

/// What to do with one run dir entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    /// Not ours (not a sandbox id, another backend, mismatched id) or owner alive.
    Keep,
    /// No (valid) meta yet and still young: a spawn may be in progress.
    KeepYoung,
    /// No meta and stale: remove the directory only.
    RemoveStale,
    /// Owner dead: kill the helper if still alive, remove cgroup + dir.
    Reap(Box<RunMeta>),
}

fn looks_like_sandbox_id(name: &str) -> bool {
    name.len() == 36
        && name.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

fn judge(name: &str, meta: io::Result<RunMeta>, age: Option<Duration>) -> Verdict {
    if !looks_like_sandbox_id(name) {
        return Verdict::Keep;
    }
    match meta {
        Ok(m) if m.backend == META_BACKEND && m.sandbox_id.to_string() == name => {
            if m.owner.is_alive() {
                Verdict::Keep
            } else {
                Verdict::Reap(Box::new(m))
            }
        }
        Ok(_) => Verdict::Keep, // someone else's directory
        Err(_) if age.is_some_and(|a| a > STALE_AFTER) => Verdict::RemoveStale,
        Err(_) => Verdict::KeepYoung,
    }
}

/// Reap orphans under `run_dir` (see the module docs). Sandboxes of live owners, including
/// this process, are never touched.
pub fn reap_run_dir(run_dir: &Path) -> Result<ReapReport> {
    let mut report = ReapReport::default();
    let entries = match fs::read_dir(run_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(io_at(run_dir, e)),
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(md) = fs::symlink_metadata(&dir) else {
            continue;
        };
        if !md.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let age = md.modified().ok().and_then(|m| m.elapsed().ok());
        match judge(&name, RunMeta::load(&dir), age) {
            Verdict::Keep | Verdict::KeepYoung => continue,
            Verdict::RemoveStale => {}
            Verdict::Reap(meta) => {
                if let Some(helper) = &meta.helper
                    && helper.signal(Signal::KILL, true)
                {
                    report.killed.push(helper.pid);
                    // Let the kernel tear the VMM down before touching its files.
                    for _ in 0..50 {
                        if !helper.is_alive() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                #[cfg(target_os = "linux")]
                if let Some(cg) = &meta.cgroup {
                    crate::cgroup::CgroupLeaf { path: cg.clone() }.remove();
                }
            }
        }
        match fs::remove_dir_all(&dir) {
            Ok(()) => report.run_dirs.push(dir),
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "cannot remove orphan run dir");
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;

    use super::*;

    #[test]
    fn parses_proc_stat_with_odd_comm() {
        let stat =
            "1234 (krun (x)) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 3 0 987654 1000 10";
        let i = parse_proc_stat(stat).unwrap();
        assert_eq!(i.start, "987654");
        assert!(!i.zombie);
        let z = "9 (a) Z 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 55 0 0";
        assert!(parse_proc_stat(z).unwrap().zombie);
        assert!(parse_proc_stat("garbage").is_none());
    }

    #[test]
    fn parses_ps_line() {
        let i = parse_ps_line("Ss   Tue Nov 24 10:00:00 2026\n").unwrap();
        assert_eq!(i.start, "Tue Nov 24 10:00:00 2026");
        assert!(!i.zombie);
        assert!(parse_ps_line("Z+ Tue Nov 24 10:00:00 2026").unwrap().zombie);
        assert!(parse_ps_line("").is_none());
        assert!(parse_ps_line("S").is_none());
    }

    #[test]
    fn identity_is_pid_plus_start_time() {
        let me = ProcId::current();
        assert!(me.is_alive());
        assert!(me.start.is_some(), "start time readable on this host");
        let impostor = ProcId {
            pid: me.pid,
            start: Some("not-a-start-time".into()),
        };
        assert!(!impostor.is_alive(), "same pid, different start time");
        assert!(
            !impostor.signal(Signal::KILL, false),
            "never signals a reused pid"
        );
        assert!(ProcId::of(u32::MAX).is_none());
    }

    #[test]
    fn signals_a_live_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let id = ProcId::of(child.id()).unwrap();
        assert!(id.is_alive());
        assert!(id.signal(Signal::KILL, false));
        child.wait().unwrap();
        assert!(!id.is_alive());
    }

    #[test]
    fn judge_matrix() {
        let id = SandboxId::new();
        let name = id.to_string();
        let old = Some(STALE_AFTER * 2);
        let young = Some(Duration::from_secs(1));
        let missing = || Err(io::Error::from(io::ErrorKind::NotFound));

        assert_eq!(judge("net", missing(), old), Verdict::Keep);
        assert_eq!(judge(&name, missing(), young), Verdict::KeepYoung);
        assert_eq!(judge(&name, missing(), None), Verdict::KeepYoung);
        assert_eq!(judge(&name, missing(), old), Verdict::RemoveStale);

        let live = RunMeta::new(id);
        assert_eq!(judge(&name, Ok(live.clone()), old), Verdict::Keep);

        let mut foreign = live.clone();
        foreign.backend = "firecracker".into();
        assert_eq!(judge(&name, Ok(foreign), old), Verdict::Keep);
        let other = SandboxId::new().to_string();
        assert_eq!(judge(&other, Ok(live.clone()), old), Verdict::Keep);

        let mut dead = live;
        dead.owner.start = Some("long ago".into());
        assert!(matches!(judge(&name, Ok(dead), young), Verdict::Reap(_)));
    }

    #[test]
    fn reaps_dead_owner_keeps_live_and_foreign() {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("run");

        // Live owner (this process): untouched.
        let (live, _) = create_run_dir(&run, SandboxId::new()).unwrap();

        // Dead owner with a still-running "helper" in its own process group.
        let mut helper = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let (dead, mut meta) = create_run_dir(&run, SandboxId::new()).unwrap();
        meta.owner.start = Some("a process that is gone".into());
        meta.helper = ProcId::of(helper.id());
        meta.save(&dead).unwrap();

        // Dead owner whose helper pid was reused: dir removed, nothing signalled.
        let (reused, mut meta2) = create_run_dir(&run, SandboxId::new()).unwrap();
        meta2.owner.start = Some("gone".into());
        meta2.helper = Some(ProcId {
            pid: std::process::id(),
            start: Some("not me".into()),
        });
        meta2.save(&reused).unwrap();

        // Foreign / non-id entries and a fresh dir without meta: untouched.
        fs::create_dir_all(run.join("net")).unwrap();
        let fresh = run.join(SandboxId::new().to_string());
        fs::create_dir(&fresh).unwrap();
        fs::write(run.join("stray-file"), "x").unwrap();

        let r = reap_run_dir(&run).unwrap();
        let mut got = r.run_dirs.clone();
        got.sort();
        let mut want = vec![dead.clone(), reused.clone()];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(r.killed, [helper.id()]);
        assert!(
            helper.wait().unwrap().code().is_none(),
            "killed by a signal"
        );
        assert!(live.is_dir() && fresh.is_dir() && run.join("net").is_dir());

        assert!(reap_run_dir(&run).unwrap().is_empty());
        assert!(
            reap_run_dir(&tmp.path().join("missing"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn run_dir_is_private_and_never_reused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        let (dir, meta) = create_run_dir(&tmp.path().join("krun"), id).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(RunMeta::load(&dir).unwrap(), meta);
        assert!(create_run_dir(&tmp.path().join("krun"), id).is_err());
    }
}
