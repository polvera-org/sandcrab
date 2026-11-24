//! Run dir, helper spawn, lifeline pipe, readiness, timings and crash watch (design §2, §7,
//! §10).
//!
//! Lifecycle of one sandbox:
//!
//! 1. `<run_dir>/<id>/` (0700) with `meta.json` (owner pid + start time) — [`crate::reap`].
//! 2. `sandcrab-krun` is spawned in its own process group with a cleared environment (only
//!    the libkrunfw directory on `DYLD_FALLBACK_LIBRARY_PATH` / `LD_LIBRARY_PATH`), stdout and
//!    stderr appended to `vmm.log`, and **stdin piped**. Its identity is recorded in
//!    `meta.json`; on Linux it is moved into its cgroup leaf. Only then is the
//!    [`HelperConfig`](crate::HelperConfig) line written. stdin is never closed afterwards: it
//!    is the **lifeline** (EOF → the helper SIGKILLs itself), so a dead owner never leaves a
//!    running VM behind.
//! 3. A tokio task owns the `Child` and publishes its exit on a `watch` channel (**crash
//!    watch**). Every readiness step races against it, so a helper that dies during boot fails
//!    the spawn at once with its exit code meaning and the tail of `vmm.log`; after readiness,
//!    the connector fails fast with "vm exited".
//! 4. Readiness: `ctl.sock` answers `ping` → `init.configure` → `v.sock` answers `hello`.
//! 5. Teardown ladder: `init.shutdown` → wait until `term_after` → SIGTERM the group → wait
//!    until `kill_after` → SIGKILL the group (and drop the lifeline) → reap → remove the
//!    cgroup leaf and the run dir. `destroy` returns only once the helper is gone, so the
//!    manager releases workspace leases only when nothing can write to them any more.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rustix::process::{Pid, Signal, kill_process_group};
use sandcrab_core::{
    BoxedStream, Connector, Endpoint, LifecycleTimings, PortForwards, PublishedPort, Result,
    SandboxError, SandboxHandle, SandboxId,
};
use sandcrab_protocol::init::{InitConfig, InitConfigureResult};
use tokio::io::AsyncWriteExt;
use tokio::process::{ChildStdin, Command};
use tokio::sync::watch;

use crate::config::{CONTROL_SOCKET_NAME, LibkrunConfig, TOOLS_SOCKET_NAME};
use crate::control::connect_control;
use crate::helper_proto::exit_code;
use crate::reap::{ProcId, RunMeta, io_at};

pub(crate) const BACKEND: &str = "libkrun";
/// Helper stdout + stderr (libkrun logs land here too).
pub const LOG_FILE: &str = "vmm.log";
/// Per-sandbox scratch disk inside the run dir.
pub const SCRATCH_FILE: &str = "scratch.ext4";

/// Environment variable through which libkrun finds libkrunfw (it `dlopen`s it by leaf name).
#[cfg(target_os = "macos")]
pub const LIB_PATH_ENV: &str = "DYLD_FALLBACK_LIBRARY_PATH";
#[cfg(not(target_os = "macos"))]
pub const LIB_PATH_ENV: &str = "LD_LIBRARY_PATH";

pub(crate) fn krun_err(context: &str, err: impl std::fmt::Display) -> SandboxError {
    SandboxError::Backend {
        backend: BACKEND.into(),
        message: format!("{context}: {err}"),
    }
}

/// How the helper process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl HelperExit {
    fn from_status(status: &ExitStatus) -> Self {
        use std::os::unix::process::ExitStatusExt;
        Self {
            code: status.code(),
            signal: status.signal(),
        }
    }

    /// The status could not be collected (the watcher went away).
    fn unknown() -> Self {
        Self {
            code: None,
            signal: None,
        }
    }
}

impl std::fmt::Display for HelperExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.code, self.signal) {
            (Some(c), _) => match exit_code::describe(c) {
                Some(d) => write!(f, "exit code {c} ({d})"),
                None => write!(f, "exit code {c}"),
            },
            (None, Some(s)) => write!(f, "killed by signal {s}"),
            (None, None) => f.write_str("unknown exit status"),
        }
    }
}

pub(crate) type ExitRx = watch::Receiver<Option<HelperExit>>;

/// Wait until the helper has exited.
pub(crate) async fn until_exit(mut rx: ExitRx) -> HelperExit {
    let seen = rx.wait_for(Option::is_some).await.map(|v| v.clone()).ok();
    seen.flatten()
        .or_else(|| rx.borrow().clone())
        .unwrap_or_else(HelperExit::unknown)
}

/// Last `max` bytes of `vmm.log`.
#[must_use]
pub fn log_tail(dir: &Path, max: usize) -> String {
    let Ok(data) = fs::read(dir.join(LOG_FILE)) else {
        return String::new();
    };
    let start = data.len().saturating_sub(max);
    String::from_utf8_lossy(&data[start..]).trim().to_owned()
}

fn with_log(dir: &Path, msg: String) -> String {
    let tail = log_tail(dir, 2048);
    if tail.is_empty() {
        msg
    } else {
        format!("{msg}; {LOG_FILE}: {tail}")
    }
}

#[must_use]
pub(crate) fn unix_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
}

/// Process-level inputs of [`KrunVm::launch`].
#[derive(Debug, Clone)]
pub(crate) struct Launch<'a> {
    pub helper: &'a Path,
    /// Directory holding libkrunfw (exported as [`LIB_PATH_ENV`]).
    pub lib_dir: &'a Path,
    /// The validated config line (`HelperConfig::to_line`).
    pub config_line: &'a [u8],
    /// Linux: `(parent, memory.max, pids.max)` of the cgroup leaf.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub cgroup: Option<(&'a Path, u64, u32)>,
}

/// Everything a running (or half-started) VM owns. Dropping it without [`KrunVm::stop`]
/// SIGKILLs the helper group, drops the lifeline and removes the run dir.
pub(crate) struct KrunVm {
    pub id: SandboxId,
    pub dir: PathBuf,
    pub meta: RunMeta,
    pid: Option<u32>,
    lifeline: Option<ChildStdin>,
    exit: Option<ExitRx>,
    #[cfg(target_os = "linux")]
    cgroup: Option<crate::cgroup::CgroupLeaf>,
    /// Cleanup ran, so `Drop` does nothing.
    done: bool,
}

impl std::fmt::Debug for KrunVm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KrunVm")
            .field("id", &self.id)
            .field("dir", &self.dir)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl KrunVm {
    pub fn new(id: SandboxId, dir: PathBuf, meta: RunMeta) -> Self {
        Self {
            id,
            dir,
            meta,
            pid: None,
            lifeline: None,
            exit: None,
            #[cfg(target_os = "linux")]
            cgroup: None,
            done: false,
        }
    }

    pub fn control_socket(&self) -> PathBuf {
        self.dir.join(CONTROL_SOCKET_NAME)
    }

    pub fn tools_socket(&self) -> PathBuf {
        self.dir.join(TOOLS_SOCKET_NAME)
    }

    pub fn launched(&self) -> bool {
        self.pid.is_some()
    }

    /// Spawn the helper, record it, put it in its cgroup leaf, hand it the config and start
    /// the crash watch. The lifeline (stdin) stays open until teardown.
    pub async fn launch(&mut self, l: &Launch<'_>) -> Result<()> {
        let log_path = self.dir.join(LOG_FILE);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| io_at(&log_path, e))?;
        let log2 = log.try_clone().map_err(|e| io_at(&log_path, e))?;
        let mut cmd = Command::new(l.helper);
        cmd.current_dir(&self.dir)
            .env_clear()
            .env(LIB_PATH_ENV, l.lib_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log2))
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| krun_err(&format!("cannot launch helper `{}`", l.helper.display()), e))?;
        let pid = child
            .id()
            .ok_or_else(|| krun_err("helper", "exited immediately"))?;
        self.pid = Some(pid);
        let stdin = child.stdin.take().expect("stdin is piped");

        // Crash watch: the task owns the child and reaps it.
        let (tx, rx) = watch::channel(None);
        tokio::spawn(async move {
            let exit = match child.wait().await {
                Ok(s) => HelperExit::from_status(&s),
                Err(e) => {
                    tracing::warn!(error = %e, "waiting for the libkrun helper failed");
                    HelperExit::unknown()
                }
            };
            tx.send_replace(Some(exit));
        });
        self.exit = Some(rx);

        self.meta.helper = Some(ProcId::of(pid).unwrap_or(ProcId { pid, start: None }));
        self.meta.save(&self.dir)?;

        #[cfg(target_os = "linux")]
        if let Some((parent, mem, pids)) = l.cgroup {
            let leaf = crate::cgroup::CgroupLeaf::create(parent, self.id, mem, pids)?;
            self.meta.cgroup = Some(leaf.path.clone());
            let added = leaf.add(pid);
            self.cgroup = Some(leaf);
            added?;
            self.meta.save(&self.dir)?;
        }

        // The helper blocks on this line before touching libkrun.
        let mut stdin = stdin;
        let write = async {
            stdin.write_all(l.config_line).await?;
            stdin.flush().await
        };
        if let Err(e) = write.await {
            // EPIPE: the helper died before reading its config.
            let why = match self.wait_exit(Duration::from_millis(500)).await {
                Some(exit) => exit.to_string(),
                None => e.to_string(),
            };
            return Err(krun_err(
                "helper did not accept its config",
                with_log(&self.dir, why),
            ));
        }
        self.lifeline = Some(stdin);
        Ok(())
    }

    /// The crash watch (`None` before launch).
    pub fn exit_watch(&self) -> Option<ExitRx> {
        self.exit.clone()
    }

    /// The helper's exit, if it has exited.
    pub fn exited(&self) -> Option<HelperExit> {
        self.exit.as_ref().and_then(|rx| rx.borrow().clone())
    }

    /// Wait up to `d` for the helper to exit.
    pub async fn wait_exit(&self, d: Duration) -> Option<HelperExit> {
        let rx = self.exit.clone()?;
        tokio::time::timeout(d, until_exit(rx)).await.ok()
    }

    /// Run `fut` unless the helper exits first.
    async fn race_exit<T>(
        &self,
        what: &str,
        fut: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let Some(rx) = self.exit.clone() else {
            return fut.await;
        };
        tokio::select! {
            r = fut => r.map_err(|e| match e {
                SandboxError::Timeout { what: w, after } => SandboxError::Timeout {
                    what: with_log(&self.dir, w),
                    after,
                },
                other => other,
            }),
            exit = until_exit(rx) => Err(krun_err(
                &format!("VM exited during {what}"),
                with_log(&self.dir, exit.to_string()),
            )),
        }
    }

    /// Readiness, part 1: wait for `crabd init` on `ctl.sock`, then `init.configure`.
    pub async fn configure(
        &self,
        init: &InitConfig,
        deadline: Instant,
        timings: &LifecycleTimings,
    ) -> Result<InitConfigureResult> {
        let sock = self.control_socket();
        let (mut ctl, attempts) = self
            .race_exit("boot", connect_control(&sock, deadline))
            .await?;
        timings.mark_custom("control_connected");
        tracing::debug!(sandbox = %self.id, attempts, "guest control port answered");
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_secs(1));
        let res = self
            .race_exit("init.configure", ctl.configure(init, remaining))
            .await?;
        timings.mark_custom("init_configured");
        Ok(res)
    }

    /// Readiness, part 2: `crabd serve` answers `hello` on `v.sock`.
    pub async fn wait_tools(&self, deadline: Instant, timings: &LifecycleTimings) -> Result<()> {
        let connector = self.connector();
        let ready = self
            .race_exit("tools readiness", async {
                sandcrab_core::wait_ready(&connector, deadline).await
            })
            .await?;
        // `Ready` itself is the manager's mark (its own `hello` through the connector).
        timings.mark_custom("tools_ready");
        ready.client.close().await;
        Ok(())
    }

    pub fn connector(&self) -> KrunConnector {
        KrunConnector {
            endpoint: Endpoint::Unix(self.tools_socket()),
            exit: self.exit.clone(),
        }
    }

    fn signal_group(&self, signal: Signal) -> bool {
        // Not reaped yet ⇒ the pid (and its group) is still ours: no reuse race.
        if self.exited().is_some() {
            return false;
        }
        let Some(pid) = self
            .pid
            .and_then(|p| i32::try_from(p).ok())
            .and_then(Pid::from_raw)
        else {
            return false;
        };
        kill_process_group(pid, signal).is_ok()
    }

    /// Graceful stop ladder (design §10). Returns which step stopped the helper.
    pub async fn stop(&mut self, cfg: &LibkrunConfig) -> &'static str {
        if !self.launched() || self.exited().is_some() {
            self.lifeline = None;
            return "already exited";
        }
        let t0 = Instant::now();
        let shutdown = async {
            let budget = cfg.term_after.min(Duration::from_millis(500));
            let (mut ctl, _) = connect_control(&self.control_socket(), t0 + budget).await?;
            ctl.shutdown(cfg.shutdown_grace_ms, budget).await
        };
        if let Err(e) = shutdown.await {
            tracing::debug!(sandbox = %self.id, error = %e, "init.shutdown failed");
        }
        if self
            .wait_exit(cfg.term_after.saturating_sub(t0.elapsed()))
            .await
            .is_some()
        {
            self.lifeline = None;
            return "init.shutdown";
        }
        self.signal_group(Signal::TERM);
        if self
            .wait_exit(cfg.kill_after.saturating_sub(t0.elapsed()))
            .await
            .is_some()
        {
            self.lifeline = None;
            return "SIGTERM";
        }
        self.kill_and_wait().await;
        "SIGKILL"
    }

    /// SIGKILL the helper group, drop the lifeline, and wait for the reap.
    pub async fn kill_and_wait(&mut self) {
        self.kill();
        if self.wait_exit(Duration::from_secs(5)).await.is_none() && self.launched() {
            tracing::warn!(sandbox = %self.id, pid = ?self.pid, "libkrun helper did not exit after SIGKILL");
        }
    }

    fn kill(&mut self) {
        self.signal_group(Signal::KILL);
        // EOF on the lifeline makes the helper SIGKILL itself even if the signal raced.
        self.lifeline = None;
    }

    /// Final cleanup once the helper is gone: cgroup leaf, run dir (sockets, scratch, log).
    pub fn cleanup(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(cg) = self.cgroup.take() {
            cg.remove();
        }
        if let Err(e) = fs::remove_dir_all(&self.dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(dir = %self.dir.display(), error = %e, "cannot remove run dir");
        }
        self.done = true;
    }
}

impl Drop for KrunVm {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.kill();
        self.cleanup();
    }
}

/// Tools connector: the run dir's `v.sock`, failing fast once the VM has exited.
#[derive(Debug, Clone)]
pub struct KrunConnector {
    endpoint: Endpoint,
    exit: Option<ExitRx>,
}

#[async_trait]
impl Connector for KrunConnector {
    async fn connect(&self) -> io::Result<BoxedStream> {
        if let Some(exit) = self.exit.as_ref().and_then(|rx| rx.borrow().clone()) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("libkrun vm exited: {exit}"),
            ));
        }
        self.endpoint.connect().await
    }
}

/// Live libkrun sandbox. Dropping it without `destroy` closes the published ports (field
/// order: before the VM is killed).
pub(crate) struct KrunSandbox {
    pub ports: PortForwards,
    pub vm: KrunVm,
    pub connector: Arc<KrunConnector>,
    pub timings: LifecycleTimings,
    pub config: Arc<LibkrunConfig>,
}

impl std::fmt::Debug for KrunSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KrunSandbox")
            .field("vm", &self.vm)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl SandboxHandle for KrunSandbox {
    fn id(&self) -> SandboxId {
        self.vm.id
    }

    fn connector(&self) -> Arc<dyn Connector> {
        self.connector.clone()
    }

    fn published_ports(&self) -> Vec<PublishedPort> {
        self.ports.published().to_vec()
    }

    fn timings(&self) -> LifecycleTimings {
        self.timings.clone()
    }

    async fn destroy(mut self: Box<Self>) -> Result<()> {
        let t = self.timings.clone();
        // Stop accepting first: no new tunnels into a VM that is going away.
        std::mem::replace(&mut self.ports, PortForwards::none())
            .shutdown()
            .await;
        if let Some(exit) = self.vm.exited() {
            tracing::warn!(sandbox = %self.vm.id, %exit, "libkrun VM had already exited");
        }
        let cfg = Arc::clone(&self.config);
        let how = self.vm.stop(&cfg).await;
        t.mark_custom("vmm_exited");
        tracing::debug!(sandbox = %self.vm.id, how, "libkrun helper stopped");
        // Live virtio-fs: no workspace export, the writes already landed on the host.
        self.vm.cleanup();
        t.mark_custom("cleanup_done");
        Ok(())
    }
}

/// Create the scratch disk at `dest` (`size_mib` MiB, ext4, label `scratch`).
///
/// macOS: a blank image cached per size under `<cache_root>/krun-scratch/` is copied with
/// `std::fs::copy`, which clones on APFS (instant copy-on-write) and needs `mke2fs` only
/// once. Linux: a fresh sparse image straight from `mke2fs` (lazy inode tables, a few ms).
pub(crate) fn scratch_disk(cache_root: &Path, size_mib: u64, dest: &Path) -> Result<()> {
    let rootfs_err = |e: sandcrab_rootfs::Error| krun_err("scratch disk", e);
    if cfg!(target_os = "macos") {
        let dir = cache_root.join("krun-scratch");
        fs::create_dir_all(&dir).map_err(|e| io_at(&dir, e))?;
        let blank = dir.join(format!("blank-{size_mib}.ext4"));
        if !blank.is_file() {
            let tmp = dir.join(format!(
                ".blank-{size_mib}.{}.{}.tmp",
                std::process::id(),
                SandboxId::new()
            ));
            sandcrab_rootfs::create_scratch(&tmp, size_mib).map_err(rootfs_err)?;
            fs::rename(&tmp, &blank).map_err(|e| io_at(&blank, e))?;
        }
        fs::copy(&blank, dest).map_err(|e| io_at(dest, e))?;
        Ok(())
    } else {
        sandcrab_rootfs::create_scratch(dest, size_mib)
            .map(drop)
            .map_err(rootfs_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_descriptions() {
        let e = HelperExit {
            code: Some(exit_code::PIN_MISMATCH),
            signal: None,
        };
        assert_eq!(
            e.to_string(),
            "exit code 71 (pinned artifact SHA-256 mismatch)"
        );
        let e = HelperExit {
            code: Some(126),
            signal: None,
        };
        assert!(e.to_string().contains("guest init failed"));
        let e = HelperExit {
            code: Some(3),
            signal: None,
        };
        assert_eq!(e.to_string(), "exit code 3");
        let e = HelperExit {
            code: None,
            signal: Some(9),
        };
        assert_eq!(e.to_string(), "killed by signal 9");
        assert_eq!(HelperExit::unknown().to_string(), "unknown exit status");
    }

    #[test]
    fn log_tail_is_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(log_tail(tmp.path(), 10), "");
        fs::write(tmp.path().join(LOG_FILE), "0123456789abcdef\n").unwrap();
        assert_eq!(log_tail(tmp.path(), 7), "abcdef");
        assert!(with_log(tmp.path(), "boom".into()).ends_with("vmm.log: 0123456789abcdef"));
    }

    #[tokio::test]
    async fn dead_vm_connector_fails_fast() {
        let (tx, rx) = watch::channel(None);
        let c = KrunConnector {
            endpoint: Endpoint::Unix("/nonexistent/v.sock".into()),
            exit: Some(rx),
        };
        let err = c.connect().await.err().unwrap();
        assert_ne!(err.kind(), io::ErrorKind::ConnectionAborted);
        tx.send_replace(Some(HelperExit {
            code: Some(1),
            signal: None,
        }));
        let err = c.connect().await.err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
        assert!(err.to_string().contains("vm exited: exit code 1"), "{err}");
    }

    /// A run dir under /tmp (the macOS $TMPDIR is too long for socket paths) and a
    /// shell-script stand-in for `sandcrab-krun`.
    fn fake(script: &str) -> (tempfile::TempDir, KrunVm, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let helper = tmp.path().join("fake-krun");
        fs::write(&helper, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        let (dir, meta) =
            crate::reap::create_run_dir(&tmp.path().join("run"), SandboxId::new()).unwrap();
        (tmp, KrunVm::new(meta.sandbox_id, dir, meta), helper)
    }

    /// Launch the fake helper. A test thread may fork while another test's script is still
    /// open for writing; the child holds that fd until its own exec, so exec'ing the script
    /// can fail with ETXTBSY for a moment (the inode is the same whatever its name). Retry.
    async fn launch_fake(vm: &mut KrunVm, helper: &Path, lib_dir: &Path) {
        for _ in 0..50 {
            match vm.launch(&launch(helper, lib_dir)).await {
                Ok(()) => return,
                Err(e) if e.to_string().contains("Text file busy") => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("launch: {e}"),
            }
        }
        panic!("fake helper stayed busy (ETXTBSY)");
    }

    fn launch<'a>(helper: &'a Path, lib_dir: &'a Path) -> Launch<'a> {
        Launch {
            helper,
            lib_dir,
            config_line: b"{\"version\":1}\n",
            cgroup: None,
        }
    }

    #[tokio::test]
    async fn crash_during_boot_maps_the_exit_code() {
        let (tmp, mut vm, helper) = fake(
            "read line; echo \"got: $line lib=$DYLD_FALLBACK_LIBRARY_PATH$LD_LIBRARY_PATH\"; exit 71",
        );
        launch_fake(&mut vm, &helper, tmp.path()).await;
        let meta = RunMeta::load(&vm.dir).unwrap();
        assert_eq!(meta.helper.as_ref().map(|p| p.pid), vm.pid);
        let init = crate::control::tests::init_cfg();
        let t = LifecycleTimings::start();
        let err = vm
            .configure(&init, Instant::now() + Duration::from_secs(10), &t)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("VM exited during boot"), "{err}");
        assert!(
            err.contains("exit code 71 (pinned artifact SHA-256 mismatch)"),
            "{err}"
        );
        assert!(
            err.contains(r#"got: {"version":1}"#),
            "config line reached the helper: {err}"
        );
        // SIP-protected /bin/sh strips DYLD_* on macOS (the real, unprotected helper keeps it).
        if cfg!(not(target_os = "macos")) {
            assert!(
                err.contains(&format!("lib={}", tmp.path().display())),
                "{err}"
            );
        }
        assert!(vm.connector().connect().await.is_err());
        assert_eq!(
            vm.stop(&LibkrunConfig::with_home(tmp.path(), "/c")).await,
            "already exited"
        );
        vm.cleanup();
        assert!(!vm.dir.exists());
    }

    #[tokio::test]
    async fn stop_escalates_to_sigterm() {
        // Never answers the control port; dies on SIGTERM.
        let (tmp, mut vm, helper) = fake("read line; exec sleep 30");
        launch_fake(&mut vm, &helper, tmp.path()).await;
        let mut cfg = LibkrunConfig::with_home(tmp.path(), "/c");
        cfg.term_after = Duration::from_millis(100);
        cfg.kill_after = Duration::from_millis(2000);
        assert_eq!(vm.stop(&cfg).await, "SIGTERM");
        assert_eq!(vm.exited().unwrap().signal, Some(15));
    }

    #[tokio::test]
    async fn stop_escalates_to_sigkill() {
        let (tmp, mut vm, helper) =
            fake("trap '' TERM; read line; touch armed; while :; do sleep 0.05; done");
        launch_fake(&mut vm, &helper, tmp.path()).await;
        // The config write returns before the script ran `trap`: wait until it is armed.
        for _ in 0..500 {
            if vm.dir.join("armed").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut cfg = LibkrunConfig::with_home(tmp.path(), "/c");
        cfg.term_after = Duration::from_millis(100);
        cfg.kill_after = Duration::from_millis(300);
        let how = vm.stop(&cfg).await;
        assert_eq!(
            how,
            "SIGKILL",
            "exit: {:?}; log: {}",
            vm.exited(),
            log_tail(&vm.dir, 500)
        );
        assert_eq!(vm.exited().unwrap().signal, Some(9));
    }

    #[tokio::test]
    async fn drop_kills_and_cleans_up() {
        let (tmp, mut vm, helper) = fake("read line; exec sleep 30");
        launch_fake(&mut vm, &helper, tmp.path()).await;
        let id = vm.meta.helper.clone().unwrap();
        let dir = vm.dir.clone();
        assert!(id.is_alive());
        drop(vm);
        for _ in 0..200 {
            if !id.is_alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!id.is_alive(), "helper killed on drop");
        assert!(!dir.exists(), "run dir removed on drop");
    }

    #[tokio::test]
    async fn until_exit_handles_a_vanished_watcher() {
        let (tx, rx) = watch::channel(None);
        drop(tx);
        assert_eq!(until_exit(rx).await, HelperExit::unknown());
    }
}
