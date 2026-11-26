//! Self-test: earn the isolation claim of one template on this host by probing real sandboxes
//! (design §8), instead of repeating the backend's static capabilities.
//!
//! [`run`] spawns two fresh sandboxes of the named template: **A** with the network
//! `disabled`, one forward, and one read-only and one read-write host mount in a scratch
//! directory; **B** plain. It starts `crabd probe <line>` in them as ordinary programs (the
//! `crabd` every sandbox contains, so an image without a shell can be tested), plays the host
//! half of each check (the forward's target, the file canaries, the tree kill) and judges
//! every answer into a [`SelftestLine`]. Whatever happens, it destroys both sandboxes and
//! removes the scratch directory before it returns, also when the run is dropped half-way (a
//! deadline, a caller that went away).
//!
//! **A timeout is never a pass.** Each probe has a deadline ([`PROBE_DEADLINE`]). A line is
//! `pass` only when the probe answered with the refusal (the `errno`) the wall must produce;
//! a different answer is `fail`; a deadline, a probe that never started or an answer missing
//! is `inconclusive`. The report is `ok` only when every required line is `pass`.
//!
//! The lines, in order, are [`LINES`].
//!
//! ```no_run
//! # async fn demo(sandcrab: &sandcrab_sdk::Sandcrab) -> sandcrab_sdk::Result<()> {
//! use sandcrab_sdk::selftest::{self, SelftestConfig};
//!
//! let report = selftest::run(sandcrab, SelftestConfig::new("base")).await?;
//! if !report.ok {
//!     eprintln!("isolation not proven: {}", report.reason.unwrap_or_default());
//! }
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use sandcrab_core::{
    BackendKind, MountMode, NetworkConfig, NetworkPolicy, Result, SandboxConfig, SandboxError,
    UserSpec,
};
use sandcrab_protocol::Overflow;
use sandcrab_protocol::api::SelftestStatus::{Fail, Inconclusive, Pass};
use sandcrab_protocol::api::{SelftestLine, SelftestReport, SelftestStatus};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::{Output, OutputStream, ProcessSpec, Sandbox, Sandcrab};

/// Deadline of one probe. Passing it makes the line `inconclusive`.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(5);

/// The forward port of sandbox A.
pub const FORWARD_PORT: u16 = 47_101;
/// A loopback port of A that is not forwarded.
pub const UNDECLARED_PORT: u16 = 47_102;
/// The port sandbox B listens on.
pub const OTHER_PORT: u16 = 47_103;

/// Every line of a report, in order.
pub const LINES: &[&str] = &[
    "caps-empty",
    "no-new-privs",
    "userns-refused",
    "net-loopback-only",
    "net-external-refused",
    "dns-refused",
    "gateway-refused",
    "metadata-refused",
    "forward-reaches-target",
    "forward-only-declared",
    "forward-fails-closed",
    "fs-outside-mounts",
    "other-sandbox-unreachable",
    "exec-env-exact",
    "tree-kill",
];

/// What to test.
#[derive(Debug, Clone)]
pub struct SelftestConfig {
    /// The template the claim is about.
    pub template: String,
    /// Where the scratch directory with the mount canaries is made. It must lie under a
    /// read-write root of the [`crate::MountPolicy`]. Default: the policy's first
    /// read-write root.
    pub scratch_root: Option<PathBuf>,
    /// Host paths that must not exist in the sandbox, on top of the scratch canary and the
    /// state directory (a server adds its socket path).
    pub hidden_paths: Vec<PathBuf>,
    /// Sandbox A's network. `disabled` (the default) is what the claim is about; anything
    /// else makes the network lines fail, which is how a test shows the self-test can fail.
    pub network: NetworkPolicy,
    /// The uid:gid both sandboxes run as. A server passes its caller's, so the claim is about
    /// the setup that caller gets. `None`: the template's (or backend's) default user. The
    /// read-write scratch directory is handed to this user.
    pub user: Option<UserSpec>,
}

impl SelftestConfig {
    #[must_use]
    pub fn new(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
            scratch_root: None,
            hidden_paths: Vec::new(),
            network: NetworkPolicy::Disabled,
            user: None,
        }
    }
}

/// How a probe ended, as the host saw it.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeRun {
    /// The JSON line the probe printed.
    Answer(Value),
    /// The deadline passed.
    Timeout,
    /// No line: the probe could not start, died, or printed something else.
    NoAnswer(String),
}

/// What the host side observed, for the lines whose verdict needs it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostFacts {
    /// `forward-reaches-target`: the nonce sent, and what the target received.
    pub forward_nonce: String,
    pub target_got: Option<String>,
    /// `forward-only-declared`: connections the target saw during that probe.
    pub stray_connections: u64,
    /// `fs-outside-mounts`: the canary's content, the nonce the probe wrote through the rw
    /// mount and what the host then read from that file.
    pub canary: String,
    pub rw_nonce: String,
    pub rw_on_host: Option<String>,
    /// `other-sandbox-unreachable`: B answered that it listens.
    pub other_listening: bool,
    /// `exec-env-exact`: the environment given.
    pub env: BTreeMap<String, String>,
    /// `tree-kill`: `tree_clean` of the exit after the cancel (`None`: no exit record in
    /// time), and the second probe looking for the nonce.
    pub tree_clean: Option<bool>,
    pub leftover: Option<ProbeRun>,
}

/// Spawn the sandboxes, run every line, destroy everything, and report. Fails only when the
/// sandboxes or the scratch directory cannot be made (the template does not exist, no
/// read-write mount root, the backend refused).
pub async fn run(sandcrab: &Sandcrab, config: SelftestConfig) -> Result<SelftestReport> {
    let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let start = Instant::now();
    let scratch = Scratch::create(sandcrab, &config)?;

    let base = || {
        let c = SandboxConfig::from_template(&config.template);
        match config.user {
            Some(u) => c.user(u),
            None => c,
        }
    };
    let a_config = SandboxConfig {
        network: Some(NetworkConfig::from(config.network)),
        ..base()
            .forward(FORWARD_PORT)
            .host_mount(scratch.ro(), MountMode::ReadOnly)
            .host_mount(scratch.rw(), MountMode::ReadWrite)
    };
    let a = sandcrab.spawn(a_config).await;
    let b = match &a {
        Ok(_) => Some(sandcrab.spawn(base()).await),
        Err(_) => None,
    };
    let spawned = start.elapsed();
    let result = match (a, b) {
        (Ok(a), Some(Ok(b))) => {
            let mut hidden = vec![scratch.outside(), sandcrab.state_dir().to_path_buf()];
            hidden.extend(config.hidden_paths.iter().cloned());
            let lines = probe_all(&a, &b, &scratch, &hidden).await;
            let probed = start.elapsed();
            destroy(a, b).await;
            // Where the time went, for a run that is slower than expected.
            tracing::info!(
                spawn_ms = spawned.as_millis(),
                probes_ms = (probed - spawned).as_millis(),
                destroy_ms = (start.elapsed() - probed).as_millis(),
                "self-test timings"
            );
            Ok(lines)
        }
        (Ok(a), Some(Err(e))) => {
            let _ = a.destroy().await;
            Err(e)
        }
        (Err(e), _) => Err(e),
        (Ok(_), None) => unreachable!("B is spawned whenever A is"),
    };
    drop(scratch);
    let lines = result?;
    Ok(assemble(
        sandcrab.backend_kind().to_string(),
        config.template,
        started_at,
        start.elapsed(),
        lines,
    ))
}

async fn destroy(a: Sandbox, b: Sandbox) {
    let (ra, rb) = tokio::join!(a.destroy(), b.destroy());
    for r in [ra, rb] {
        if let Err(e) = r {
            tracing::warn!(error = %e, "self-test: destroying a sandbox failed");
        }
    }
}

/// `ok`, `reason` and the totals from the lines.
#[must_use]
pub fn assemble(
    backend: String,
    template: String,
    started_at: String,
    duration: Duration,
    lines: Vec<SelftestLine>,
) -> SelftestReport {
    let first_bad = lines
        .iter()
        .find(|l| l.required && l.status != SelftestStatus::Pass);
    SelftestReport {
        ok: first_bad.is_none() && !lines.is_empty(),
        reason: match first_bad {
            Some(l) => Some(format!(
                "{} is {}: {}",
                l.id,
                status_name(l.status),
                l.detail
            )),
            None if lines.is_empty() => Some("no line was run".into()),
            None => None,
        },
        backend,
        template,
        started_at,
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        lines,
    }
}

/// `pass`, `fail` or `inconclusive`.
#[must_use]
pub fn status_name(s: SelftestStatus) -> &'static str {
    match s {
        SelftestStatus::Pass => "pass",
        SelftestStatus::Fail => "fail",
        SelftestStatus::Inconclusive => "inconclusive",
    }
}

// ---------------------------------------------------------------------------------------------
// Scratch directory
// ---------------------------------------------------------------------------------------------

/// `<root>/sandcrab-selftest-<id>/{ro/canary, rw/, canary-outside}`; `ro` and `rw` are
/// mounted, `canary-outside` sits beside them and must not be visible. Removed on drop, so a
/// run that is dropped half-way leaves nothing behind.
struct Scratch {
    dir: PathBuf,
    canary: String,
}

impl Scratch {
    fn create(sandcrab: &Sandcrab, config: &SelftestConfig) -> Result<Self> {
        let root = match &config.scratch_root {
            Some(r) => std::fs::canonicalize(r).map_err(|e| io_at(r, e))?,
            None => sandcrab
                .manager()
                .mount_policy()
                .and_then(|p| {
                    p.roots()
                        .into_iter()
                        .find(|r| r.max_mode == MountMode::ReadWrite)
                })
                .map(|r| r.path)
                .ok_or_else(|| {
                    SandboxError::InvalidConfig(
                        "the self-test needs a read-write mount root (a mount policy) for its \
                         scratch directory"
                            .into(),
                    )
                })?,
        };
        let id = Uuid::new_v4().simple().to_string();
        let dir = root.join(format!("sandcrab-selftest-{}", &id[..12]));
        let canary = format!("canary-{id}");
        let s = Self { dir, canary };
        let made = (|| {
            std::fs::create_dir(&s.dir)?;
            std::fs::create_dir(s.ro())?;
            std::fs::create_dir(s.rw())?;
            std::fs::write(s.ro().join("canary"), &s.canary)?;
            std::fs::write(s.outside(), &s.canary)?;
            // The sandbox user must be able to write through the rw mount.
            if let Some(u) = config.user {
                use std::os::unix::fs::MetadataExt;
                let rw = s.rw();
                let meta = std::fs::metadata(&rw)?;
                if meta.uid() != u.uid || meta.gid() != u.gid {
                    std::os::unix::fs::chown(&rw, Some(u.uid), Some(u.gid))?;
                }
            }
            Ok(())
        })();
        // On an error `s` drops here and removes what was made.
        made.map_err(|e| io_at(&s.dir, e))?;
        Ok(s)
    }

    fn ro(&self) -> PathBuf {
        self.dir.join("ro")
    }

    fn rw(&self) -> PathBuf {
        self.dir.join("rw")
    }

    fn outside(&self) -> PathBuf {
        self.dir.join("canary-outside")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(dir = %self.dir.display(), error = %e, "self-test: cannot remove scratch");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Running probes
// ---------------------------------------------------------------------------------------------

/// `crabd` inside a sandbox of `kind`.
#[must_use]
pub fn guest_crabd(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Docker => "/opt/sandcrab/crabd",
        BackendKind::Firecracker | BackendKind::CloudHypervisor | BackendKind::Libkrun => {
            "/sbin/crabd"
        }
    }
}

fn probe_spec(sb: &Sandbox, args: &[&str]) -> ProcessSpec {
    let argv = [guest_crabd(sb.backend()), "probe"]
        .into_iter()
        .chain(args.iter().copied());
    ProcessSpec::new(argv)
        .timeout(PROBE_DEADLINE)
        .kill_grace(Duration::from_millis(200))
        .output_cap(256 * 1024, Overflow::Kill)
}

/// The first line of `stdout` as JSON.
fn first_line(stdout: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(stdout);
    serde_json::from_str(text.lines().next()?).ok()
}

/// Run one probe to its end.
async fn probe(sb: &Sandbox, spec: ProcessSpec) -> ProbeRun {
    // The guest times the deadline; the host's own bound covers a guest that never answers.
    let host_bound = PROBE_DEADLINE + Duration::from_secs(2);
    let out = match tokio::time::timeout(host_bound, sb.exec(spec)).await {
        Err(_) => return ProbeRun::Timeout,
        Ok(Err(e)) => return ProbeRun::NoAnswer(e.to_string()),
        Ok(Ok(out)) => out,
    };
    if matches!(out.outcome, crate::ProcessOutcome::TimedOut(_)) {
        return ProbeRun::Timeout;
    }
    match first_line(&out.stdout) {
        Some(v) if out.outcome.success() => ProbeRun::Answer(v),
        _ => ProbeRun::NoAnswer(format!(
            "{:?}; stderr: {}",
            out.outcome,
            out.outcome.stderr_tail().trim()
        )),
    }
}

/// Read a running probe's output until its first line (or the deadline).
async fn read_first_line(out: &mut OutputStream) -> ProbeRun {
    let read = async {
        let mut buf = Vec::new();
        while let Some(chunk) = out.next().await {
            let (Output::Stdout(d) | Output::Stderr(d)) = chunk;
            buf.extend_from_slice(&d);
            if buf.contains(&b'\n') {
                return first_line(&buf).map_or_else(
                    || ProbeRun::NoAnswer(String::from_utf8_lossy(&buf).into_owned()),
                    ProbeRun::Answer,
                );
            }
        }
        ProbeRun::NoAnswer("the probe ended without a line".into())
    };
    tokio::time::timeout(PROBE_DEADLINE, read)
        .await
        .unwrap_or(ProbeRun::Timeout)
}

/// The host end of A's forward: records what it gets and answers `ack:<data>`; refuses
/// when `refuse` is set.
#[derive(Default)]
struct Target {
    connections: AtomicU64,
    refuse: AtomicBool,
    got: Mutex<Option<String>>,
}

impl Target {
    fn serve(self: &Arc<Self>, sb: &Sandbox) -> Result<()> {
        let target = Arc::clone(self);
        sb.serve_forwards_with(move |port| {
            let target = Arc::clone(&target);
            async move {
                target.connections.fetch_add(1, Ordering::SeqCst);
                if target.refuse.load(Ordering::SeqCst) || port != FORWARD_PORT {
                    return Err(io::Error::other("refused by the self-test"));
                }
                let (mut mine, theirs) = tokio::io::duplex(64 * 1024);
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut limited = (&mut mine).take(4096);
                    let read = limited.read_to_end(&mut data);
                    if tokio::time::timeout(PROBE_DEADLINE, read).await.is_err() {
                        return;
                    }
                    let data = String::from_utf8_lossy(&data).into_owned();
                    let reply = format!("ack:{data}");
                    *target.got.lock().unwrap_or_else(PoisonError::into_inner) = Some(data);
                    let _ = mine.write_all(reply.as_bytes()).await;
                    let _ = mine.shutdown().await;
                });
                Ok(theirs)
            }
        })
    }
}

fn io_at(path: &Path, e: io::Error) -> SandboxError {
    SandboxError::Io(io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

fn nonce() -> String {
    format!("selftest-{}", Uuid::new_v4().simple())
}

async fn probe_all(
    a: &Sandbox,
    b: &Sandbox,
    scratch: &Scratch,
    hidden: &[PathBuf],
) -> Vec<SelftestLine> {
    let mut facts = HostFacts::default();
    let mut runs: Vec<(&'static str, ProbeRun)> = Vec::with_capacity(LINES.len());

    for id in [
        "caps-empty",
        "no-new-privs",
        "userns-refused",
        "net-loopback-only",
        "net-external-refused",
        "dns-refused",
        "gateway-refused",
        "metadata-refused",
    ] {
        runs.push((id, probe(a, probe_spec(a, &[id])).await));
    }

    // Forwards.
    let target = Arc::new(Target::default());
    if let Err(e) = target.serve(a) {
        tracing::warn!(error = %e, "self-test: cannot serve forwards");
    }
    let deadline = Instant::now() + PROBE_DEADLINE;
    while !a.forwards_listening() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let port = FORWARD_PORT.to_string();
    facts.forward_nonce = nonce();
    let spec = probe_spec(a, &["forward-reaches-target", &port, &facts.forward_nonce]);
    runs.push(("forward-reaches-target", probe(a, spec).await));
    facts.target_got = target
        .got
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();

    let before = target.connections.load(Ordering::SeqCst);
    let undeclared = UNDECLARED_PORT.to_string();
    let spec = probe_spec(a, &["forward-only-declared", &undeclared]);
    runs.push(("forward-only-declared", probe(a, spec).await));
    facts.stray_connections = target.connections.load(Ordering::SeqCst) - before;

    target.refuse.store(true, Ordering::SeqCst);
    let spec = probe_spec(a, &["forward-fails-closed", &port, &nonce()]);
    runs.push(("forward-fails-closed", probe(a, spec).await));

    // Mounts.
    facts.canary.clone_from(&scratch.canary);
    facts.rw_nonce = nonce();
    let rw_file = scratch.rw().join("probe-rw");
    let mut args: Vec<String> = vec!["fs-outside-mounts".into()];
    for p in hidden {
        args.extend(["--absent".into(), p.display().to_string()]);
    }
    args.extend([
        "--read".into(),
        scratch.ro().join("canary").display().to_string(),
        "--ro".into(),
        scratch.ro().display().to_string(),
        "--rw".into(),
        rw_file.display().to_string(),
        facts.rw_nonce.clone(),
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    runs.push(("fs-outside-mounts", probe(a, probe_spec(a, &args)).await));
    facts.rw_on_host = std::fs::read_to_string(&rw_file).ok();

    // The other sandbox.
    let other_nonce = nonce();
    let other_port = OTHER_PORT.to_string();
    let listener = b.spawn(
        probe_spec(b, &["listen", &other_port, &other_nonce]).timeout(Duration::from_secs(60)),
    );
    let mut listener_handle = None;
    if let Ok((process, mut out)) = listener {
        let first = read_first_line(&mut out).await;
        facts.other_listening =
            matches!(&first, ProbeRun::Answer(v) if v["listening"] == Value::Bool(true));
        listener_handle = Some((process, out));
    }
    let spec = probe_spec(a, &["other-sandbox-unreachable", &other_port, &other_nonce]);
    runs.push(("other-sandbox-unreachable", probe(a, spec).await));
    drop(listener_handle); // stops the listener

    // The exact environment.
    facts.env = BTreeMap::from([
        ("SANDCRAB_SELFTEST".to_owned(), nonce()),
        ("LC_ALL".to_owned(), "C".to_owned()),
    ]);
    let mut spec = probe_spec(a, &["exec-env-exact"]);
    spec.env.clone_from(&facts.env);
    runs.push(("exec-env-exact", probe(a, spec).await));

    // Tree kill.
    let (run, clean, leftover) = tree_kill(a).await;
    facts.tree_clean = clean;
    facts.leftover = leftover;
    runs.push(("tree-kill", run));

    runs.into_iter()
        .map(|(id, run)| evaluate(id, &run, &facts))
        .collect()
}

/// Start `tree-kill`, wait for its grandchild, cancel with a short grace, read `tree_clean`,
/// then look for the nonce again.
async fn tree_kill(a: &Sandbox) -> (ProbeRun, Option<bool>, Option<ProbeRun>) {
    let n = nonce();
    let spec = probe_spec(a, &["tree-kill", &n]).timeout(Duration::from_secs(60));
    let (process, mut out) = match a.spawn(spec) {
        Ok(p) => p,
        Err(e) => return (ProbeRun::NoAnswer(e.to_string()), None, None),
    };
    let first = read_first_line(&mut out).await;
    drop(out);
    if !matches!(first, ProbeRun::Answer(_)) {
        return (first, None, None);
    }
    let _ = process.cancel(Some(Duration::from_millis(300))).await;
    let clean = tokio::time::timeout(PROBE_DEADLINE, process.exit())
        .await
        .ok()
        .and_then(|o| o.exit_record().map(|e| e.tree_clean));
    let leftover = probe(a, probe_spec(a, &["find", &n])).await;
    (first, clean, Some(leftover))
}

// ---------------------------------------------------------------------------------------------
// Judging
// ---------------------------------------------------------------------------------------------

const UNREACHABLE: &[&str] = &["ENETUNREACH", "EHOSTUNREACH"];

fn line(
    id: &str,
    status: SelftestStatus,
    detail: impl Into<String>,
    evidence: Value,
) -> SelftestLine {
    SelftestLine {
        id: id.to_owned(),
        status,
        required: true,
        detail: detail.into(),
        evidence,
    }
}

/// Judge one line from its probe and the host's facts.
#[must_use]
pub fn evaluate(id: &str, run: &ProbeRun, facts: &HostFacts) -> SelftestLine {
    let answer = match run {
        ProbeRun::Answer(v) => v,
        ProbeRun::Timeout => {
            return line(
                id,
                SelftestStatus::Inconclusive,
                format!("the probe hit its {} s deadline", PROBE_DEADLINE.as_secs()),
                json!({ "probe": "timeout" }),
            );
        }
        ProbeRun::NoAnswer(why) => {
            return line(
                id,
                SelftestStatus::Inconclusive,
                "the probe gave no answer",
                json!({ "probe": null, "error": why }),
            );
        }
    };
    let (status, detail, host) = judge(id, answer, facts);
    let evidence = if host.is_null() {
        json!({ "probe": answer })
    } else {
        json!({ "probe": answer, "host": host })
    };
    line(id, status, detail, evidence)
}

fn judge(id: &str, v: &Value, f: &HostFacts) -> (SelftestStatus, String, Value) {
    let none = Value::Null;
    match id {
        "caps-empty" => {
            let fields = ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"];
            let masks: Vec<Option<u64>> = fields
                .iter()
                .map(|k| {
                    v["status"][k]
                        .as_str()
                        .and_then(|s| u64::from_str_radix(s, 16).ok())
                })
                .collect();
            if masks.iter().any(Option::is_none) {
                (
                    Inconclusive,
                    "a capability set could not be read".into(),
                    none,
                )
            } else if masks.iter().all(|m| *m == Some(0)) {
                (Pass, "every capability set is empty".into(), none)
            } else {
                (Fail, "a capability set is not empty".into(), none)
            }
        }
        "no-new-privs" => match v["status"]["NoNewPrivs"].as_str() {
            Some("1") => (Pass, "no_new_privs is set".into(), none),
            Some(_) => (Fail, "no_new_privs is not set".into(), none),
            None => (Inconclusive, "NoNewPrivs could not be read".into(), none),
        },
        "userns-refused" => match v["result"].as_str() {
            Some("EPERM" | "ENOSPC" | "EINVAL") => {
                (Pass, "creating a user namespace is refused".into(), none)
            }
            Some("ok") => (Fail, "a user namespace was created".into(), none),
            Some(r) => (Fail, format!("unshare gave {r}, not a refusal"), none),
            None => (Inconclusive, "no result".into(), none),
        },
        "net-loopback-only" => match v["interfaces"].as_array() {
            Some(i) if i.len() == 1 && i[0] == "lo" => {
                (Pass, "the only interface is lo".into(), none)
            }
            // An interface that is down carries nothing, and with no capabilities (`caps-empty`)
            // the sandbox cannot bring it up (libkrun's kernel has a `dummy0`, always down).
            Some(i)
                if v["up"]
                    .as_array()
                    .is_some_and(|u| u.iter().all(|n| n == "lo")) =>
            {
                (
                    Pass,
                    format!(
                        "the only interface up is lo (down: {})",
                        Value::from(i.iter().filter(|n| *n != "lo").cloned().collect::<Vec<_>>())
                    ),
                    none,
                )
            }
            Some(i) => (
                Fail,
                format!("interfaces besides lo: {}", Value::from(i.clone())),
                none,
            ),
            None => (
                Inconclusive,
                "the interfaces could not be read".into(),
                none,
            ),
        },
        "net-external-refused" | "gateway-refused" | "metadata-refused" => attempts(
            v,
            |_target, r| UNREACHABLE.contains(&r),
            "every address is unreachable",
        ),
        "dns-refused" => attempts(
            v,
            |target, r| {
                let ip = target
                    .rsplit(' ')
                    .next()
                    .and_then(|a| a.parse::<std::net::SocketAddr>().ok())
                    .map(|a| a.ip());
                UNREACHABLE.contains(&r)
                    || (r == "ECONNREFUSED" && ip.is_some_and(|ip| ip.is_loopback()))
                    // No IPv6 at all is no way out either.
                    || (matches!(r, "EAFNOSUPPORT" | "EADDRNOTAVAIL")
                        && ip.is_some_and(|ip| ip.is_ipv6()))
            },
            "no resolver is reachable",
        ),
        "forward-reaches-target" => {
            let host = json!({ "sent": f.forward_nonce, "target_got": f.target_got });
            let expected = format!("ack:{}", f.forward_nonce);
            match (v["attempt"]["result"].as_str(), v["end"].as_str()) {
                (Some("ok"), _)
                    if v["reply"] == expected.as_str()
                        && f.target_got.as_deref() == Some(f.forward_nonce.as_str()) =>
                {
                    (
                        Pass,
                        "the target got the nonce and the probe its answer".into(),
                        host,
                    )
                }
                (Some("timeout"), _) | (Some("ok"), Some("timeout")) => {
                    (Inconclusive, "the exchange hit its deadline".into(), host)
                }
                (Some(r), _) => (
                    Fail,
                    format!("the forward did not carry the exchange ({r})"),
                    host,
                ),
                (None, _) => (Inconclusive, "no result".into(), host),
            }
        }
        "forward-only-declared" => {
            let host = json!({ "target_connections": f.stray_connections });
            match v["attempt"]["result"].as_str() {
                Some("ECONNREFUSED") if f.stray_connections == 0 => {
                    (Pass, "an undeclared port is refused".into(), host)
                }
                Some("ECONNREFUSED") => (Fail, "the target saw a connection".into(), host),
                Some("timeout") => (Inconclusive, "the connect hit its deadline".into(), host),
                Some(r) => (Fail, format!("an undeclared port gave {r}"), host),
                None => (Inconclusive, "no result".into(), host),
            }
        }
        "forward-fails-closed" => match (v["attempt"]["result"].as_str(), v["end"].as_str()) {
            (Some("ECONNREFUSED"), _) => (Pass, "the connection was refused".into(), none),
            (Some("ok"), Some("eof" | "ECONNRESET" | "EPIPE")) if v["bytes"] == 0 => (
                Pass,
                "the connection was closed with nothing delivered".into(),
                none,
            ),
            (Some("timeout"), _) | (Some("ok"), Some("timeout")) => {
                (Inconclusive, "the exchange hit its deadline".into(), none)
            }
            (Some(r), end) => (
                Fail,
                format!("a refused target did not close the connection empty ({r}, {end:?})"),
                none,
            ),
            (None, _) => (Inconclusive, "no result".into(), none),
        },
        "fs-outside-mounts" => fs_outside_mounts(v, f),
        "other-sandbox-unreachable" => {
            let host = json!({ "other_listening": f.other_listening });
            let visible = v["visible"].as_array().map(Vec::len);
            match (v["attempt"]["result"].as_str(), visible) {
                _ if !f.other_listening => (
                    Inconclusive,
                    "the other sandbox never listened".into(),
                    host,
                ),
                (Some("ECONNREFUSED"), Some(0)) => (
                    Pass,
                    "the other sandbox's port and processes are out of reach".into(),
                    host,
                ),
                (Some("timeout"), _) => (Inconclusive, "the connect hit its deadline".into(), host),
                (Some(r), Some(n)) => (
                    Fail,
                    format!("the other sandbox: connect gave {r}, {n} of its processes visible"),
                    host,
                ),
                _ => (Inconclusive, "no result".into(), host),
            }
        }
        "exec-env-exact" => match v["env"].as_object() {
            Some(env) => {
                let got: BTreeMap<String, String> = env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                    .collect();
                if got == f.env {
                    (
                        Pass,
                        "the environment is exactly the one given".into(),
                        none,
                    )
                } else {
                    let extra: Vec<&String> =
                        got.keys().filter(|k| !f.env.contains_key(*k)).collect();
                    (
                        Fail,
                        format!("the environment differs from the one given (extra: {extra:?})"),
                        json!({ "given": f.env.keys().collect::<Vec<_>>() }),
                    )
                }
            }
            None => (Inconclusive, "no environment".into(), none),
        },
        "tree-kill" => {
            let leftover = match &f.leftover {
                Some(ProbeRun::Answer(l)) => l["visible"].as_array().map(Vec::len),
                _ => None,
            };
            let host = json!({ "tree_clean": f.tree_clean, "leftover": leftover });
            match (v["ready"].as_bool(), f.tree_clean, leftover) {
                (Some(true), Some(true), Some(0)) => {
                    (Pass, "the cancel left no process behind".into(), host)
                }
                (Some(true), Some(false), _) => {
                    (Fail, "the exit says tree_clean: false".into(), host)
                }
                (Some(true), _, Some(n)) if n > 0 => {
                    (Fail, format!("{n} processes survived the cancel"), host)
                }
                (Some(false), ..) => (Inconclusive, "the grandchild did not start".into(), host),
                _ => (
                    Inconclusive,
                    "the cancel or the second look did not finish".into(),
                    host,
                ),
            }
        }
        other => (Inconclusive, format!("unknown line {other}"), none),
    }
}

/// Judge a list of `attempts`: a disallowed result fails, else a timeout is inconclusive.
fn attempts(
    v: &Value,
    allowed: impl Fn(&str, &str) -> bool,
    pass: &str,
) -> (SelftestStatus, String, Value) {
    let Some(list) = v["attempts"].as_array().filter(|l| !l.is_empty()) else {
        return (Inconclusive, "nothing was tried".into(), Value::Null);
    };
    let mut timeout = false;
    for a in list {
        let target = a["target"].as_str().unwrap_or_default();
        match a["result"].as_str() {
            Some(r) if allowed(target, r) => {}
            Some("timeout") => timeout = true,
            Some(r) => return (Fail, format!("{target} gave {r}"), Value::Null),
            None => timeout = true,
        }
    }
    if timeout {
        (
            Inconclusive,
            "an attempt hit its deadline".into(),
            Value::Null,
        )
    } else {
        (Pass, pass.into(), Value::Null)
    }
}

fn fs_outside_mounts(v: &Value, f: &HostFacts) -> (SelftestStatus, String, Value) {
    let host = json!({ "rw_seen_on_host": f.rw_on_host.as_deref() == Some(f.rw_nonce.as_str()) });
    let Some(absent) = v["absent"].as_array().filter(|a| !a.is_empty()) else {
        return (Inconclusive, "nothing was looked up".into(), host);
    };
    if let Some(seen) = absent.iter().find(|a| a["result"] != "ENOENT") {
        return (
            Fail,
            format!(
                "{} is reachable from the sandbox ({})",
                seen["path"].as_str().unwrap_or_default(),
                seen["result"].as_str().unwrap_or_default()
            ),
            host,
        );
    }
    if v["read"]["result"] != "ok" || v["read"]["content"] != f.canary.as_str() {
        return (
            Fail,
            "the canary in the read-only mount was not readable".into(),
            host,
        );
    }
    match v["ro_write"]["result"].as_str() {
        Some("EROFS") => {}
        Some(r) => {
            return (
                Fail,
                format!("a write to the read-only mount gave {r}, not EROFS"),
                host,
            );
        }
        None => {
            return (
                Inconclusive,
                "the read-only write was not tried".into(),
                host,
            );
        }
    }
    if v["rw_write"]["result"] != "ok" || f.rw_on_host.as_deref() != Some(f.rw_nonce.as_str()) {
        return (
            Fail,
            "a write through the read-write mount did not reach the host".into(),
            host,
        );
    }
    (
        Pass,
        "only the mounts are visible; read-only refuses writes, read-write accepts them".into(),
        host,
    )
}

#[cfg(test)]
mod tests;
