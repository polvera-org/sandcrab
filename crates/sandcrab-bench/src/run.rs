//! The benchmark run: matrix → seeded cell order → ABAB-interleaved iterations → JSON.
//!
//! Timed windows wrap the public SDK calls only (`Sandcrab::spawn`, `Sandbox::*`,
//! `Sandbox::destroy`); backend probes (inspect, cgroup reads) happen outside them, except
//! the teardown verification poll that the spec puts inside `teardown`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use sandcrab_sdk::{
    FileReadParams, GrepParams, ListDirParams, MountMode, NetworkPolicy, Sandbox, SandboxConfig,
    TimingMarks,
};

use sandcrab_sdk::protocol::EntryKind;

use crate::backend::{BackendId, LiveBackend, OpenOpts, Probe};
use crate::fixtures::{self, Fixtures};
use crate::host;
use crate::results::{
    Availability, BackendStatus, Cell, CellStatus, DensityRun, DensityStep, DriftCheck, Failure,
    HarnessOverhead, Metric, Phase, RunConfig, RunResult, SCHEMA_VERSION, TemplateInfo, Unit,
};
use crate::sample;
use crate::schedule;
use crate::stats;

/// Spec minimum for measured iterations per cell.
pub const MIN_N: usize = 30;
/// A cell with more failures than this fraction is unreliable (spec §5.12).
pub const UNRELIABLE_RATE: f64 = 0.01;
/// Drift threshold on `spawn_ready` p50 (spec §5.11).
pub const DRIFT_PCT: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scenario {
    Lifecycle,
    Calls,
    AgentTurn,
    Resources,
    Density,
    Drift,
}

impl Scenario {
    pub const ALL: [Self; 6] = [
        Self::Lifecycle,
        Self::Calls,
        Self::AgentTurn,
        Self::Resources,
        Self::Density,
        Self::Drift,
    ];

    pub fn parse(s: &str) -> Result<Vec<Self>, String> {
        Ok(match s.trim() {
            "all" => Self::ALL.to_vec(),
            "lifecycle" => vec![Self::Lifecycle],
            "calls" => vec![Self::Calls],
            "agent-turn" | "agent_turn" => vec![Self::AgentTurn],
            "resources" => vec![Self::Resources],
            "density" => vec![Self::Density],
            "drift" => vec![Self::Drift],
            other => {
                return Err(format!(
                    "unknown scenario `{other}` (all | lifecycle | calls | agent-turn | \
                     resources | density | drift)"
                ));
            }
        })
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Lifecycle => "lifecycle",
            Self::Calls => "calls",
            Self::AgentTurn => "agent-turn",
            Self::Resources => "resources",
            Self::Density => "density",
            Self::Drift => "drift",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ws {
    W0,
    W1,
    W2,
}

impl Ws {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_uppercase().as_str() {
            "W0" => Ok(Self::W0),
            "W1" => Ok(Self::W1),
            "W2" => Ok(Self::W2),
            other => Err(format!("unknown workspace `{other}` (W0 | W1 | W2)")),
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::W0 => "W0",
            Self::W1 => "W1",
            Self::W2 => "W2",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cache {
    Warm,
    Cold,
    ColdUncached,
}

impl Cache {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "warm" => Ok(Self::Warm),
            "cold" => Ok(Self::Cold),
            "cold-uncached" => Ok(Self::ColdUncached),
            other => Err(format!(
                "unknown cache mode `{other}` (warm | cold | cold-uncached)"
            )),
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Cold => "cold",
            Self::ColdUncached => "cold-uncached",
        }
    }
}

/// Density memory budget.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Budget {
    /// Fraction of `MemTotal`, minus the memory already used at the start of the rep.
    PctOfTotal(f64),
    Bytes(u64),
}

impl Budget {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if let Some(p) = s.strip_suffix('%') {
            let v: f64 = p
                .parse()
                .map_err(|e| format!("density budget `{s}`: {e}"))?;
            if !(0.0..=100.0).contains(&v) {
                return Err(format!("density budget `{s}` must be within 0–100%"));
            }
            return Ok(Self::PctOfTotal(v / 100.0));
        }
        let (num, mult) = [
            ("GiB", 1u64 << 30),
            ("MiB", 1 << 20),
            ("G", 1 << 30),
            ("M", 1 << 20),
        ]
        .iter()
        .find_map(|(suf, m)| s.strip_suffix(suf).map(|n| (n, *m)))
        .unwrap_or((s, 1));
        let v: u64 = num
            .trim()
            .parse()
            .map_err(|e| format!("density budget `{s}`: {e}"))?;
        Ok(Self::Bytes(v * mult))
    }

    /// Budget in bytes given `MemTotal` and the current `MemAvailable`.
    #[must_use]
    pub fn bytes(self, mem_total: u64, mem_available: u64) -> u64 {
        match self {
            Self::PctOfTotal(f) => {
                let used = mem_total.saturating_sub(mem_available);
                ((mem_total as f64 * f) as u64).saturating_sub(used)
            }
            Self::Bytes(b) => b,
        }
    }
}

/// Everything a run needs, resolved from the CLI.
#[derive(Debug, Clone)]
pub struct Plan {
    pub argv: Vec<String>,
    pub backends: Vec<BackendId>,
    pub templates: Vec<String>,
    pub scenarios: BTreeSet<Scenario>,
    pub workspaces: Vec<Ws>,
    pub caches: Vec<Cache>,
    pub runs: usize,
    pub warmups: usize,
    pub calls: usize,
    pub call_warmups: usize,
    pub resource_runs: usize,
    pub idle_window: Duration,
    pub settle: Duration,
    pub density_budget: Budget,
    pub density_budget_raw: String,
    pub density_cap: usize,
    pub density_reps: usize,
    pub density_floor: u64,
    pub touch_mib: u64,
    pub quick: bool,
    pub allow_small_n: bool,
    pub only: Option<String>,
    pub seed: u64,
    pub out_dir: PathBuf,
    pub host_label: Option<String>,
    pub kind_override: Option<String>,
    pub noise_declared: String,
    pub max_load: f64,
    pub w2_files: usize,
    pub w2_bytes: u64,
    pub work_dir: PathBuf,
    pub templates_dir: PathBuf,
    pub crabd: PathBuf,
}

/// One cell of the matrix (backend-independent; backends are interleaved inside it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellSpec {
    pub template: String,
    pub cache: Cache,
    pub ws: Ws,
    pub scenario: Scenario,
}

impl CellSpec {
    fn key(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.template,
            self.cache.label(),
            self.ws.label(),
            self.scenario.label()
        )
    }
}

/// The spec §3 matrix, filtered by the plan. `base` gets every cache × workspace; other
/// templates (`ts-dev`) run warm × {W0, W2} only.
#[must_use]
pub fn matrix(plan: &Plan) -> Vec<CellSpec> {
    let mut cells = Vec::new();
    let lifecycle =
        plan.scenarios.contains(&Scenario::Lifecycle) || plan.scenarios.contains(&Scenario::Calls);
    for t in &plan.templates {
        let primary = t == "base";
        let caches: Vec<Cache> = plan
            .caches
            .iter()
            .copied()
            .filter(|c| primary || *c == Cache::Warm)
            .collect();
        if lifecycle {
            for &cache in &caches {
                for &ws in &plan.workspaces {
                    if !primary && ws == Ws::W1 {
                        continue;
                    }
                    cells.push(CellSpec {
                        template: t.clone(),
                        cache,
                        ws,
                        scenario: Scenario::Lifecycle,
                    });
                }
            }
        }
        if plan.scenarios.contains(&Scenario::AgentTurn) {
            for &cache in &caches {
                cells.push(CellSpec {
                    template: t.clone(),
                    cache,
                    ws: Ws::W1,
                    scenario: Scenario::AgentTurn,
                });
            }
        }
        if plan.scenarios.contains(&Scenario::Resources) {
            cells.push(CellSpec {
                template: t.clone(),
                cache: Cache::Warm,
                ws: Ws::W1,
                scenario: Scenario::Resources,
            });
        }
    }
    if let Some(only) = &plan.only {
        cells.retain(|c| c.key().contains(only.as_str()));
    }
    cells
}

// ---------------------------------------------------------------------------------------
// per-iteration accumulation
// ---------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct IterOut {
    metrics: Vec<(String, Unit, f64)>,
    phases: Vec<(String, f64)>,
    not_measured: Vec<(String, String)>,
    failure: Option<(String, String)>,
}

impl IterOut {
    fn ns(&mut self, id: &str, d: Duration) {
        self.metrics
            .push((id.to_owned(), Unit::Ns, d.as_nanos() as f64));
    }
    fn bytes(&mut self, id: &str, v: u64) {
        self.metrics.push((id.to_owned(), Unit::Bytes, v as f64));
    }
    fn cpu(&mut self, id: &str, s: f64) {
        self.metrics.push((id.to_owned(), Unit::CpuS, s));
    }
    fn skip(&mut self, id: &str, why: impl Into<String>) {
        self.not_measured.push((id.to_owned(), why.into()));
    }
    fn fail(&mut self, id: &str, err: impl std::fmt::Display) {
        if self.failure.is_none() {
            self.failure = Some((id.to_owned(), err.to_string()));
        }
    }
    fn phases_from(&mut self, marks: &TimingMarks) {
        let td = marks.teardown_started;
        for (name, at) in &marks.custom {
            match td {
                Some(t0) if *at >= t0 => self
                    .phases
                    .push((format!("teardown.{name}"), (*at - t0).as_nanos() as f64)),
                _ => self.phases.push((
                    format!("spawn.{name}"),
                    (*at - marks.spawn_requested).as_nanos() as f64,
                )),
            }
        }
    }
}

#[derive(Debug, Default)]
struct Acc {
    metrics: BTreeMap<String, (Unit, Vec<f64>)>,
    phases: BTreeMap<String, Vec<f64>>,
    not_measured: BTreeMap<String, String>,
    failures: Vec<Failure>,
    n: usize,
}

impl Acc {
    fn add(&mut self, iter: usize, out: IterOut) {
        self.n += 1;
        for (id, unit, v) in out.metrics {
            self.metrics
                .entry(id)
                .or_insert_with(|| (unit, Vec::new()))
                .1
                .push(v);
        }
        for (id, v) in out.phases {
            self.phases.entry(id).or_default().push(v);
        }
        for (id, why) in out.not_measured {
            self.not_measured.entry(id).or_insert(why);
        }
        if let Some((metric, error)) = out.failure {
            self.failures.push(Failure {
                iter,
                metric,
                error,
            });
        }
    }

    fn into_cell(self, b: &LiveBackend, spec: &CellSpec, warmup: usize, seed: u64) -> Cell {
        let mut metrics: BTreeMap<String, Metric> = self
            .metrics
            .into_iter()
            .map(|(id, (unit, samples))| {
                let st = stats::compute(&samples, seed);
                (
                    id,
                    Metric {
                        unit,
                        samples,
                        stats: st,
                        not_measured: None,
                    },
                )
            })
            .collect();
        for (id, why) in self.not_measured {
            metrics.entry(id.clone()).or_insert(Metric {
                unit: unit_of(&id),
                samples: Vec::new(),
                stats: None,
                not_measured: Some(why),
            });
        }
        let phases = self
            .phases
            .into_iter()
            .map(|(k, samples)| {
                let st = stats::compute(&samples, seed);
                (k, Phase { samples, stats: st })
            })
            .collect();
        let unreliable = self.n > 0 && self.failures.len() as f64 / self.n as f64 > UNRELIABLE_RATE;
        Cell {
            backend: b.id.label().into(),
            capabilities: Some(b.caps),
            template: spec.template.clone(),
            cache: spec.cache.label().into(),
            workspace: spec.ws.label().into(),
            scenario: spec.scenario.label().into(),
            status: CellStatus::Ok,
            reason: None,
            todo: false,
            warmup,
            n: self.n,
            failures: self.failures,
            unreliable,
            metrics,
            phases,
        }
    }
}

fn unit_of(id: &str) -> Unit {
    if id.starts_with("mem.") || id.starts_with("disk.") {
        Unit::Bytes
    } else if id.starts_with("cpu.") {
        Unit::CpuS
    } else {
        Unit::Ns
    }
}

// ---------------------------------------------------------------------------------------
// run context
// ---------------------------------------------------------------------------------------

struct Ctx<'a> {
    plan: &'a Plan,
    live: Vec<LiveBackend>,
    fixtures: Fixtures,
    /// Per live backend: the registered W2 workspace name.
    w2_names: Vec<String>,
    dockerd_pid: Option<u32>,
    can_drop_caches: bool,
    crabd_version: OnceLock<String>,
    images: BTreeMap<String, String>,
}

/// A per-iteration workspace lease (fresh W1 copy; shared read-only W2).
struct WsLease {
    w1: Option<(String, PathBuf)>,
    w2: Option<String>,
}

impl WsLease {
    fn prepare(ctx: &Ctx<'_>, bi: usize, ws: Ws) -> Result<Self, String> {
        let mut lease = Self { w1: None, w2: None };
        if ws == Ws::W0 {
            return Ok(lease);
        }
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let name = format!("bw1-{}", &tag[..12]);
        let dir = ctx.fixtures.root.join("runs").join(&name);
        fixtures::copy_dir(&ctx.fixtures.w1_template, &dir)
            .map_err(|e| format!("copy W1 fixture: {e}"))?;
        ctx.live[bi]
            .sc
            .workspaces()
            .register_host_dir(&name, &dir)
            .map_err(|e| format!("register W1: {e}"))?;
        lease.w1 = Some((name, dir));
        if ws == Ws::W2 {
            lease.w2 = Some(ctx.w2_names[bi].clone());
        }
        Ok(lease)
    }

    fn config(&self, template: &str) -> SandboxConfig {
        let mut c = SandboxConfig::from_template(template).network(NetworkPolicy::Disabled);
        if let Some((name, _)) = &self.w1 {
            c = c
                .mount(name.as_str(), "/workspace", MountMode::ReadWrite)
                .default_cwd("/workspace");
        }
        if let Some(name) = &self.w2 {
            c = c.mount(name.as_str(), "/data", MountMode::ReadOnly);
        }
        c
    }

    fn host_dir(&self) -> Option<&PathBuf> {
        self.w1.as_ref().map(|(_, d)| d)
    }

    fn cleanup(self, b: &LiveBackend) {
        if let Some((name, dir)) = self.w1 {
            let _ = b.sc.workspaces().remove(name.as_str(), false);
            let _ = fs::remove_dir_all(dir);
        }
    }
}

fn drop_caches() -> Result<(), String> {
    let _ = std::process::Command::new("sync").status();
    fs::write("/proc/sys/vm/drop_caches", "3\n").map_err(|e| format!("drop_caches: {e}"))
}

fn can_drop_caches() -> bool {
    fs::OpenOptions::new()
        .write(true)
        .open("/proc/sys/vm/drop_caches")
        .is_ok()
}

// ---------------------------------------------------------------------------------------
// iterations
// ---------------------------------------------------------------------------------------

async fn check_bash(sb: &Sandbox, cmd: &str, expect: Option<&str>) -> Result<(), String> {
    let r = sb.bash(cmd).await.map_err(|e| e.to_string())?;
    if r.exit_code != Some(0) {
        return Err(format!("`{cmd}` exited {:?}: {}", r.exit_code, r.stderr));
    }
    if let Some(e) = expect
        && r.stdout.trim() != e
    {
        return Err(format!("`{cmd}` printed {:?}, expected {e:?}", r.stdout));
    }
    Ok(())
}

async fn file_rw_1m(ctx: &Ctx<'_>, sb: &Sandbox) -> Result<(), String> {
    let path = "/tmp/bench-1m.txt";
    sb.file_write(path, ctx.fixtures.blob.text.clone())
        .await
        .map_err(|e| e.to_string())?;
    let r = sb
        .file_read_with(FileReadParams {
            path: path.into(),
            offset: None,
            limit: Some(1_000_000),
        })
        .await
        .map_err(|e| e.to_string())?;
    let got = fixtures::sha256_hex(fixtures::strip_line_numbers(&r.content).as_bytes());
    if r.truncated || got != ctx.fixtures.blob.sha256 {
        return Err(format!(
            "1 MiB read-back mismatch (truncated={}, sha256 {got})",
            r.truncated
        ));
    }
    Ok(())
}

async fn grep_fixture(ctx: &Ctx<'_>, sb: &Sandbox) -> Result<(), String> {
    let r = sb
        .grep_with(GrepParams {
            pattern: fixtures::GREP_PATTERN.into(),
            root: Some("/workspace/src".into()),
            glob: None,
            literal: false,
            case_insensitive: false,
            context_lines: None,
        })
        .await
        .map_err(|e| e.to_string())?;
    if r.truncated || r.matches.len() != ctx.fixtures.w1.handler_matches {
        return Err(format!(
            "grep found {} matches (truncated={}), expected {}",
            r.matches.len(),
            r.truncated,
            ctx.fixtures.w1.handler_matches
        ));
    }
    Ok(())
}

fn list_params(path: &str) -> ListDirParams {
    ListDirParams {
        path: Some(path.into()),
        depth: Some(1),
        respect_gitignore: false,
    }
}

fn entry_name(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// A guest write to the rw workspace that the host can only see after teardown.
struct PendingSync {
    host: PathBuf,
    token: String,
}

/// Workspace checks after ready: `ws_attach` (list_dir sees the expected entries) and
/// `ws_sync_back` (the host sees a guest write). With live host mounts (Docker) the write is
/// polled for here; otherwise (Firecracker: copied in at spawn, synced back at teardown,
/// `supports_host_dir_mounts = false`) it is returned and checked by [`check_sync_back`].
async fn ws_checks(
    ctx: &Ctx<'_>,
    sb: &Sandbox,
    live_mounts: bool,
    ws: Ws,
    lease: &WsLease,
    out: &mut IterOut,
) -> Result<Option<PendingSync>, ()> {
    if ws == Ws::W0 {
        return Ok(None);
    }
    let t = Instant::now();
    let r: Result<(), String> = async {
        let l = sb
            .list_dir_with(list_params("/workspace"))
            .await
            .map_err(|e| e.to_string())?;
        for want in fixtures::W1_TOP {
            if !l.entries.iter().any(|e| entry_name(&e.path) == want) {
                return Err(format!("W1 entry `{want}` missing from list_dir"));
            }
        }
        if ws == Ws::W2 {
            let l = sb
                .list_dir_with(list_params("/data"))
                .await
                .map_err(|e| e.to_string())?;
            let dirs = l
                .entries
                .iter()
                .filter(|e| e.kind == EntryKind::Dir && entry_name(&e.path).starts_with('d'))
                .count();
            if dirs != ctx.fixtures.w2.dirs {
                return Err(format!(
                    "W2 list_dir shows {dirs} dirs, expected {}",
                    ctx.fixtures.w2.dirs
                ));
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = r {
        out.fail("ws_attach", e);
        return Err(());
    }
    out.ns("ws_attach", t.elapsed());

    let token = uuid::Uuid::new_v4().to_string();
    if let Err(e) = sb.file_write("/workspace/.bench-sync", token.clone()).await {
        out.fail("ws_sync_back", e);
        return Err(());
    }
    let t = Instant::now();
    let host = lease
        .host_dir()
        .expect("W1/W2 lease has a host dir")
        .join(".bench-sync");
    if !live_mounts {
        return Ok(Some(PendingSync { host, token }));
    }
    loop {
        if fs::read(&host).is_ok_and(|b| b == token.as_bytes()) {
            break;
        }
        if t.elapsed() > crate::backend::VERIFY_TIMEOUT {
            out.fail("ws_sync_back", "host never saw the guest write");
            return Err(());
        }
        tokio::task::yield_now().await;
    }
    out.ns("ws_sync_back", t.elapsed());
    Ok(None)
}

/// Sync-at-teardown backends: after a successful destroy the host must hold the guest write;
/// `ws_sync_back` is the backend's sync step (teardown start → `ws_sync_done` mark).
fn check_sync_back(p: &PendingSync, marks: &TimingMarks, out: &mut IterOut) {
    if !fs::read(&p.host).is_ok_and(|b| b == p.token.as_bytes()) {
        out.fail(
            "ws_sync_back",
            "host never saw the guest write after teardown",
        );
        return;
    }
    let done = marks
        .custom
        .iter()
        .find(|(name, _)| name == "ws_sync_done")
        .map(|(_, at)| *at);
    match (marks.teardown_started, done) {
        (Some(t0), Some(at)) if at >= t0 => out.ns("ws_sync_back", at - t0),
        _ => out.skip(
            "ws_sync_back",
            "backend reports no `ws_sync_done` teardown mark",
        ),
    }
}

/// The fixed 10-call agent-turn script (spec §1.1), run with cwd `/workspace` (W1).
async fn agent_script(ctx: &Ctx<'_>, sb: &Sandbox) -> Result<(), String> {
    let e = |e: sandcrab_sdk::SandboxError| e.to_string();
    sb.list_dir(Some("/workspace")).await.map_err(e)?;
    sb.file_read("src/main.rs").await.map_err(e)?;
    sb.grep("TODO").await.map_err(e)?;
    let ed = sb
        .file_edit("src/main.rs", fixtures::EDIT_OLD, fixtures::EDIT_NEW)
        .await
        .map_err(e)?;
    if ed.replacements != 1 {
        return Err(format!("file_edit replaced {} times", ed.replacements));
    }
    check_bash(sb, "ls -la && wc -l src/*.rs", None).await?;
    sb.file_write("notes.md", ctx.fixtures.notes_4k.clone())
        .await
        .map_err(e)?;
    sb.glob("**/*.rs").await.map_err(e)?;
    check_bash(sb, "sleep 0.05 && echo done", Some("done")).await?;
    sb.file_move("notes.md", "notes2.md", false)
        .await
        .map_err(e)?;
    sb.file_delete("notes2.md", false).await.map_err(e)?;
    Ok(())
}

async fn steady_calls(ctx: &Ctx<'_>, sb: &Sandbox, ws: Ws, out: &mut IterOut) -> Result<(), ()> {
    let plan = ctx.plan;
    let grep = ws != Ws::W0;
    // discarded warm calls
    for _ in 0..plan.call_warmups {
        let r = async {
            check_bash(sb, "echo ok", Some("ok")).await?;
            file_rw_1m(ctx, sb).await?;
            if grep {
                grep_fixture(ctx, sb).await?;
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(e) = r {
            out.fail("call.warmup", e);
            return Err(());
        }
    }
    for _ in 0..plan.calls {
        let t = Instant::now();
        if let Err(e) = check_bash(sb, "echo ok", Some("ok")).await {
            out.fail("call.bash_echo", e);
            return Err(());
        }
        out.ns("call.bash_echo", t.elapsed());
    }
    for _ in 0..plan.calls {
        let t = Instant::now();
        if let Err(e) = file_rw_1m(ctx, sb).await {
            out.fail("call.file_rw_1m", e);
            return Err(());
        }
        out.ns("call.file_rw_1m", t.elapsed());
    }
    if grep {
        for _ in 0..plan.calls {
            let t = Instant::now();
            if let Err(e) = grep_fixture(ctx, sb).await {
                out.fail("call.grep", e);
                return Err(());
            }
            out.ns("call.grep", t.elapsed());
        }
    } else {
        out.skip(
            "call.grep",
            "W0 has no fixture repo mounted; grep runs in W1/W2 cells",
        );
    }
    Ok(())
}

/// CPU attributable to the sandbox so far: cgroup usage + shim ticks.
fn sandbox_cpu_s(probe: Option<&Probe>) -> Option<f64> {
    let probe = probe?;
    let cg = sample::read_cgroup(probe.cgroup.as_deref()?).ok()?;
    let shim = probe
        .shim_pid
        .and_then(sample::proc_cpu_ticks)
        .map_or(0.0, sample::ticks_to_s);
    Some(cg.cpu_usage_usec as f64 / 1e6 + shim)
}

/// Destroy and verify resources are gone; records `teardown` (+ phases). Returns the
/// destroy's timing marks on success.
async fn timed_teardown(
    b: &LiveBackend,
    sb: Sandbox,
    probe: Option<&Probe>,
    out: &mut IterOut,
) -> Option<TimingMarks> {
    let t = Instant::now();
    match sb.destroy().await {
        Ok(marks) => {
            if let Err(e) = b.verify_gone(probe).await {
                out.fail("teardown", e);
                return None;
            }
            out.ns("teardown", t.elapsed());
            out.phases_from(&marks);
            Some(marks)
        }
        Err(e) => {
            out.fail("teardown", e);
            None
        }
    }
}

async fn lifecycle_iter(ctx: &Ctx<'_>, bi: usize, spec: &CellSpec) -> IterOut {
    let b = &ctx.live[bi];
    let mut out = IterOut::default();
    let lease = match WsLease::prepare(ctx, bi, spec.ws) {
        Ok(l) => l,
        Err(e) => {
            out.fail("ws_prepare", e);
            return out;
        }
    };
    let cfg = lease.config(&spec.template);
    let dockerd0 = ctx.dockerd_pid.and_then(sample::proc_cpu_ticks);

    let t0 = Instant::now();
    let sb = match b.sc.spawn(cfg).await {
        Ok(sb) => sb,
        Err(e) => {
            out.fail("spawn_ready", e);
            lease.cleanup(b);
            return out;
        }
    };
    let t1 = Instant::now();
    out.ns("spawn_ready", t1 - t0);
    match sb.bash("true").await {
        Ok(r) if r.exit_code == Some(0) => out.ns("first_call", t1.elapsed()),
        Ok(r) => out.fail("first_call", format!("`true` exited {:?}", r.exit_code)),
        Err(e) => out.fail("first_call", e),
    }
    let _ = ctx.crabd_version.set(sb.hello().crabd_version.clone());
    let probe = b.probe(&sb).await;

    let mut pending = None;
    if out.failure.is_none() {
        let live = b.caps.supports_host_dir_mounts;
        if let Ok(p) = ws_checks(ctx, &sb, live, spec.ws, &lease, &mut out).await {
            pending = p;
            if spec.scenario != Scenario::Drift && ctx.plan.scenarios.contains(&Scenario::Calls) {
                let _ = steady_calls(ctx, &sb, spec.ws, &mut out).await;
            }
        }
    }

    match &probe {
        Some(p) => match b.disk_rw(p).await {
            Ok(v) => out.bytes("disk.per_sandbox", v),
            Err(e) => out.skip("disk.per_sandbox", e),
        },
        None => out.skip("disk.per_sandbox", "no host probe for this backend yet"),
    }
    let cpu = sandbox_cpu_s(probe.as_ref());
    let dockerd1 = ctx.dockerd_pid.and_then(sample::proc_cpu_ticks);

    if let Some(marks) = timed_teardown(b, sb, probe.as_ref(), &mut out).await
        && let Some(p) = &pending
    {
        check_sync_back(p, &marks, &mut out);
    }
    match cpu {
        Some(c) => {
            let daemon = match (dockerd0, dockerd1) {
                (Some(a), Some(z)) => sample::ticks_to_s(z.saturating_sub(a)),
                _ => 0.0,
            };
            out.cpu("cpu.lifecycle_s", c + daemon);
        }
        None => out.skip("cpu.lifecycle_s", "container cgroup not found"),
    }
    lease.cleanup(b);
    out
}

async fn agent_turn_iter(ctx: &Ctx<'_>, bi: usize, spec: &CellSpec) -> IterOut {
    let b = &ctx.live[bi];
    let mut out = IterOut::default();
    let lease = match WsLease::prepare(ctx, bi, Ws::W1) {
        Ok(l) => l,
        Err(e) => {
            out.fail("ws_prepare", e);
            return out;
        }
    };
    let cfg = lease.config(&spec.template);
    let t0 = Instant::now();
    let sb = match b.sc.spawn(cfg).await {
        Ok(sb) => sb,
        Err(e) => {
            out.fail("agent_turn", e);
            lease.cleanup(b);
            return out;
        }
    };
    // probe cost is excluded from the window
    let tp = Instant::now();
    let probe = b.probe(&sb).await;
    let probe_cost = tp.elapsed();
    if let Err(e) = agent_script(ctx, &sb).await {
        out.fail("agent_turn", e);
        let _ = sb.destroy().await;
        lease.cleanup(b);
        return out;
    }
    let mut td = IterOut::default();
    if timed_teardown(b, sb, probe.as_ref(), &mut td)
        .await
        .is_some()
    {
        out.ns("agent_turn", t0.elapsed().saturating_sub(probe_cost));
    } else if let Some((m, e)) = td.failure {
        out.fail(&m, e);
    }
    lease.cleanup(b);
    out
}

async fn guest_mem_available(sb: &Sandbox) -> Result<u64, String> {
    let r = sb
        .bash("grep MemAvailable /proc/meminfo")
        .await
        .map_err(|e| e.to_string())?;
    host::parse_meminfo(&r.stdout)
        .get("MemAvailable")
        .copied()
        .ok_or_else(|| format!("unexpected /proc/meminfo output {:?}", r.stdout))
}

fn host_pss(probe: &Probe) -> Result<u64, String> {
    let cg = probe
        .cgroup
        .as_deref()
        .ok_or("container cgroup not found")?;
    let inner = sample::cgroup_pss(cg)?;
    // libkrun: the helper *is* the VMM and lives inside its cgroup leaf (no shim).
    if probe.helper_pid.is_some() {
        return Ok(inner);
    }
    let shim = match probe.shim_pid {
        Some(p) => sample::pss(p)?,
        None => return Err("containerd-shim pid not found".into()),
    };
    Ok(inner + shim)
}

async fn resources_iter(ctx: &Ctx<'_>, bi: usize, spec: &CellSpec) -> IterOut {
    let b = &ctx.live[bi];
    let plan = ctx.plan;
    let mut out = IterOut::default();
    let lease = match WsLease::prepare(ctx, bi, Ws::W1) {
        Ok(l) => l,
        Err(e) => {
            out.fail("ws_prepare", e);
            return out;
        }
    };
    let sb = match b.sc.spawn(lease.config(&spec.template)).await {
        Ok(sb) => sb,
        Err(e) => {
            out.fail("spawn_ready", e);
            lease.cleanup(b);
            return out;
        }
    };
    let probe = b.probe(&sb).await;
    out.skip(
        "mem.vmm_overhead",
        format!(
            "guest RAM vs VMM split not measured for {} (Firecracker-only metric)",
            b.id.label()
        ),
    );
    // libkrun without a cgroup leaf (always on macOS): helper RSS, under its own metric ids.
    if let Some(pid) = probe
        .as_ref()
        .filter(|p| p.cgroup.is_none())
        .and_then(|p| p.helper_pid)
    {
        helper_rss_iter(ctx, &sb, pid, &mut out).await;
        let _ = sb.destroy().await;
        lease.cleanup(b);
        return out;
    }
    let Some(probe) = probe.filter(|p| p.cgroup.is_some()) else {
        for id in [
            "mem.host_pss_idle",
            "mem.host_pss_post",
            "cpu.idle_60s",
            "mem.cgroup_current_idle",
        ] {
            out.skip(id, "no cgroup/process probe for this backend yet");
        }
        let _ = sb.destroy().await;
        lease.cleanup(b);
        return out;
    };
    let cg = probe.cgroup.clone().expect("filtered");

    // idle = ready + settle
    tokio::time::sleep(plan.settle).await;
    let mem = sample::sample_memory(&cg, Duration::from_secs(1)).await;
    if !mem.is_empty() {
        out.bytes(
            "mem.cgroup_current_idle",
            mem.iter().sum::<u64>() / mem.len() as u64,
        );
    }
    match host_pss(&probe) {
        Ok(v) => out.bytes("mem.host_pss_idle", v),
        Err(e) => out.skip("mem.host_pss_idle", e),
    }
    match guest_mem_available(&sb).await {
        Ok(v) => out.bytes("mem.guest_available_idle", v),
        Err(e) => out.fail("mem.guest_available_idle", e),
    }
    // idle CPU over the window
    let u0 = sample::read_cgroup(&cg).map(|s| s.cpu_usage_usec);
    tokio::time::sleep(plan.idle_window).await;
    let u1 = sample::read_cgroup(&cg).map(|s| s.cpu_usage_usec);
    match (u0, u1) {
        (Ok(a), Ok(z)) => out.cpu("cpu.idle_60s", z.saturating_sub(a) as f64 / 1e6),
        (Err(e), _) | (_, Err(e)) => out.skip("cpu.idle_60s", e),
    }

    // workload: agent script + touch_mib write/read
    let touch = format!(
        "head -c {}M /dev/urandom > /tmp/bench-touch && cat /tmp/bench-touch > /dev/null",
        plan.touch_mib
    );
    let wl = async {
        agent_script(ctx, &sb).await?;
        check_bash(&sb, &touch, None).await
    }
    .await;
    if let Err(e) = wl {
        out.fail("workload", e);
    } else {
        tokio::time::sleep(plan.settle).await;
        match host_pss(&probe) {
            Ok(v) => out.bytes("mem.host_pss_post", v),
            Err(e) => out.skip("mem.host_pss_post", e),
        }
        if let Ok(s) = sample::read_cgroup(&cg) {
            out.bytes("mem.cgroup_current_post", s.memory_current);
            out.bytes("mem.cgroup_anon_post", s.anon);
            out.bytes("mem.cgroup_file_post", s.file);
            out.bytes("mem.cgroup_kernel_post", s.kernel);
            match s.memory_peak {
                Some(p) => out.bytes("mem.cgroup_peak", p),
                None => out.skip(
                    "mem.cgroup_peak",
                    "memory.peak not available (kernel < 5.19)",
                ),
            }
        }
        if let Some(shim) = probe.shim_pid {
            match sample::pss(shim) {
                Ok(v) => out.bytes("mem.shim_pss", v),
                Err(e) => out.skip("mem.shim_pss", e),
            }
        }
        match guest_mem_available(&sb).await {
            Ok(v) => out.bytes("mem.guest_available", v),
            Err(e) => out.fail("mem.guest_available", e),
        }
        match b.disk_rw(&probe).await {
            Ok(v) => out.bytes("disk.per_sandbox", v),
            Err(e) => out.skip("disk.per_sandbox", e),
        }
    }
    if let Some(image) = ctx.images.get(&spec.template) {
        match b.disk_shared(image).await {
            Ok(v) => out.bytes("disk.shared", v),
            Err(e) => out.skip("disk.shared", e),
        }
    }
    let _ = sb.destroy().await;
    lease.cleanup(b);
    out
}

/// Resource metrics for a VMM process without a cgroup (libkrun on macOS, or Linux without
/// `SANDCRAB_BENCH_KRUN_CGROUP`): helper **RSS** idle/post (`mem.helper_rss_*`, labelled RSS,
/// never PSS: shared pages count fully), the helper's CPU over the idle window
/// (`cpu.helper_idle_60s`) and the guest view. cgroup-only metrics are skipped.
async fn helper_rss_iter(ctx: &Ctx<'_>, sb: &Sandbox, pid: u32, out: &mut IterOut) {
    let plan = ctx.plan;
    let why = "no cgroup on this host (helper process recorded instead, see \
               mem.helper_rss_* and cpu.helper_idle_60s)";
    for id in [
        "mem.host_pss_idle",
        "mem.host_pss_post",
        "cpu.idle_60s",
        "mem.cgroup_current_idle",
    ] {
        out.skip(id, why);
    }
    tokio::time::sleep(plan.settle).await;
    match sample::rss(pid) {
        Ok(v) => out.bytes("mem.helper_rss_idle", v),
        Err(e) => out.skip("mem.helper_rss_idle", e),
    }
    match guest_mem_available(sb).await {
        Ok(v) => out.bytes("mem.guest_available_idle", v),
        Err(e) => out.fail("mem.guest_available_idle", e),
    }
    // idle CPU of the whole helper process (VMM + vCPU threads) over the idle window
    let c0 = sample::process_cpu_s(pid);
    tokio::time::sleep(plan.idle_window).await;
    let c1 = sample::process_cpu_s(pid);
    match (c0, c1) {
        (Ok(a), Ok(z)) => out.cpu("cpu.helper_idle_60s", (z - a).max(0.0)),
        (Err(e), _) | (_, Err(e)) => out.skip("cpu.helper_idle_60s", e),
    }
    let touch = format!(
        "head -c {}M /dev/urandom > /tmp/bench-touch && cat /tmp/bench-touch > /dev/null",
        plan.touch_mib
    );
    let wl = async {
        agent_script(ctx, sb).await?;
        check_bash(sb, &touch, None).await
    }
    .await;
    if let Err(e) = wl {
        out.fail("workload", e);
        return;
    }
    tokio::time::sleep(plan.settle).await;
    match sample::rss(pid) {
        Ok(v) => out.bytes("mem.helper_rss_post", v),
        Err(e) => out.skip("mem.helper_rss_post", e),
    }
    match guest_mem_available(sb).await {
        Ok(v) => out.bytes("mem.guest_available", v),
        Err(e) => out.fail("mem.guest_available", e),
    }
}

async fn run_iter(ctx: &Ctx<'_>, bi: usize, spec: &CellSpec) -> IterOut {
    if spec.cache == Cache::Cold
        && let Err(e) = drop_caches()
    {
        let mut out = IterOut::default();
        out.fail("drop_caches", e);
        return out;
    }
    match spec.scenario {
        Scenario::AgentTurn => agent_turn_iter(ctx, bi, spec).await,
        Scenario::Resources => resources_iter(ctx, bi, spec).await,
        _ => lifecycle_iter(ctx, bi, spec).await,
    }
}

fn placeholder_cell(
    backend: &str,
    spec: &CellSpec,
    status: CellStatus,
    reason: String,
    todo: bool,
) -> Cell {
    Cell {
        backend: backend.into(),
        capabilities: None,
        template: spec.template.clone(),
        cache: spec.cache.label().into(),
        workspace: spec.ws.label().into(),
        scenario: spec.scenario.label().into(),
        status,
        reason: Some(reason),
        todo,
        warmup: 0,
        n: 0,
        failures: Vec::new(),
        unreliable: false,
        metrics: BTreeMap::new(),
        phases: BTreeMap::new(),
    }
}

async fn run_cell(ctx: &Ctx<'_>, spec: &CellSpec, index: usize) -> Vec<Cell> {
    let plan = ctx.plan;
    if spec.cache == Cache::Cold && !ctx.can_drop_caches {
        return ctx
            .live
            .iter()
            .map(|b| {
                placeholder_cell(
                    b.id.label(),
                    spec,
                    CellStatus::NotMeasured,
                    "cold cache needs `sync; echo 3 > /proc/sys/vm/drop_caches` (root / \
                     CAP_SYS_ADMIN); rerun sandcrab-bench as root to measure"
                        .into(),
                    true,
                )
            })
            .collect();
    }
    let (n, warm) = match spec.scenario {
        Scenario::Resources => (plan.resource_runs, plan.warmups.min(1)),
        _ => (plan.runs, plan.warmups),
    };
    let k = ctx.live.len();
    eprintln!(
        "[cell {index}] {} — {warm} warmup + {n} measured × {k} backend(s)",
        spec.key()
    );
    for bi in schedule::interleave(k, warm * k, index) {
        let _ = run_iter(ctx, bi, spec).await;
    }
    let mut accs: Vec<Acc> = (0..k).map(|_| Acc::default()).collect();
    let mut iters = vec![0usize; k];
    for bi in schedule::interleave(k, n * k, index) {
        let out = run_iter(ctx, bi, spec).await;
        if let Some((m, e)) = &out.failure {
            eprintln!(
                "  ! {} iter {}: {m}: {e}",
                ctx.live[bi].id.label(),
                iters[bi]
            );
        }
        accs[bi].add(iters[bi], out);
        iters[bi] += 1;
    }
    accs.into_iter()
        .enumerate()
        .map(|(bi, acc)| acc.into_cell(&ctx.live[bi], spec, warm, plan.seed ^ index as u64))
        .collect()
}

// ---------------------------------------------------------------------------------------
// density
// ---------------------------------------------------------------------------------------

async fn density_rep(
    ctx: &Ctx<'_>,
    bi: usize,
    template: &str,
    touched: bool,
    rep: usize,
) -> DensityRun {
    let b = &ctx.live[bi];
    let plan = ctx.plan;
    let mem = host::meminfo();
    let total = mem.get("MemTotal").copied().unwrap_or(0);
    let base_avail = mem.get("MemAvailable").copied().unwrap_or(0);
    let budget = plan.density_budget.bytes(total, base_avail);
    let mut run = DensityRun {
        backend: b.id.label().into(),
        template: template.into(),
        variant: if touched { "touched" } else { "idle" }.into(),
        rep,
        budget_bytes: budget,
        cap: plan.density_cap,
        max_n: 0,
        stop_reason: "cap".into(),
        error: None,
        steps: Vec::new(),
    };
    eprintln!(
        "[density] {} {} rep {rep}: budget {} MiB, cap {}",
        b.id.label(),
        run.variant,
        budget >> 20,
        plan.density_cap
    );
    let touch = format!(
        "head -c {}M /dev/urandom > /tmp/bench-touch && cat /tmp/bench-touch > /dev/null",
        plan.touch_mib
    );
    let mut live: Vec<(Sandbox, Option<Probe>)> = Vec::new();
    for i in 0..plan.density_cap {
        let cfg = SandboxConfig::from_template(template).network(NetworkPolicy::Disabled);
        let t0 = Instant::now();
        let sb = match b.sc.spawn(cfg).await {
            Ok(sb) => sb,
            Err(e) => {
                run.stop_reason = "failure".into();
                run.error = Some(e.to_string());
                break;
            }
        };
        let spawn_ready_ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if touched && let Err(e) = check_bash(&sb, &touch, None).await {
            run.stop_reason = "failure".into();
            run.error = Some(e);
            live.push((sb, None));
            break;
        }
        let probe = b.probe(&sb).await;
        live.push((sb, probe));
        let avail = host::mem_available();
        let mut pss_total = Some(0u64);
        let mut cg_total = Some(0u64);
        for (_, p) in &live {
            let p = p.as_ref();
            pss_total = match (pss_total, p.map(host_pss)) {
                (Some(acc), Some(Ok(v))) => Some(acc + v),
                _ => None,
            };
            cg_total = match (
                cg_total,
                p.and_then(|p| p.cgroup.as_deref())
                    .and_then(|c| sample::read_cgroup(c).ok()),
            ) {
                (Some(acc), Some(s)) => Some(acc + s.memory_current),
                _ => None,
            };
        }
        run.steps.push(DensityStep {
            i,
            spawn_ready_ns,
            host_mem_available: avail,
            host_pss_total: pss_total,
            cgroup_mem_total: cg_total,
        });
        run.max_n = i + 1;
        if base_avail.saturating_sub(avail) > budget {
            run.stop_reason = "budget".into();
            break;
        }
        if avail < plan.density_floor {
            run.stop_reason = "floor".into();
            break;
        }
    }
    for (sb, _) in live {
        let _ = sb.destroy().await;
    }
    run
}

// ---------------------------------------------------------------------------------------
// the run
// ---------------------------------------------------------------------------------------

async fn noop_overhead_ns() -> f64 {
    let mut xs = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let t = Instant::now();
        std::future::ready(()).await;
        xs.push(t.elapsed().as_nanos() as f64);
    }
    xs.sort_by(f64::total_cmp);
    stats::percentile(&xs, 0.5)
}

async fn drift_p50(ctx: &Ctx<'_>, n: usize) -> Option<f64> {
    let spec = CellSpec {
        template: ctx.plan.templates.first()?.clone(),
        cache: Cache::Warm,
        ws: Ws::W0,
        scenario: Scenario::Drift,
    };
    let mut xs = Vec::new();
    for _ in 0..n {
        let out = lifecycle_iter(ctx, 0, &spec).await;
        if let Some((_, _, v)) = out.metrics.iter().find(|(id, _, _)| id == "spawn_ready") {
            xs.push(*v);
        }
    }
    xs.sort_by(f64::total_cmp);
    (!xs.is_empty()).then(|| stats::percentile(&xs, 0.5))
}

pub async fn run(plan: &Plan) -> anyhow::Result<(RunResult, PathBuf)> {
    let started = chrono::Utc::now();
    let mut notes = Vec::new();
    let mut invalid = Vec::new();

    let opts_for = |id: BackendId| OpenOpts {
        state_dir: plan.work_dir.join("state").join(id.label()),
        templates_dir: plan.templates_dir.clone(),
        crabd: plan.crabd.clone(),
    };
    let host_opts = opts_for(BackendId::Firecracker);
    let mut host_info = host::collect(
        plan.kind_override.as_deref(),
        &plan.crabd,
        &crate::backend::fc_paths(&host_opts),
    );
    let mut noise = host::noise_start(&plan.noise_declared, plan.max_load);
    eprintln!("{}", host::render(&host_info, &noise));

    // backends
    let mut live = Vec::new();
    let mut backends = Vec::new();
    for &id in &plan.backends {
        match crate::backend::open(id, &opts_for(id)).await {
            Ok(b) => {
                backends.push(BackendStatus {
                    backend: id.label().into(),
                    status: Availability::Available,
                    reason: None,
                    todo: false,
                    capabilities: Some(b.caps),
                });
                live.push(b);
            }
            Err(reason) => {
                eprintln!("backend {} unavailable: {reason}", id.label());
                notes.push(format!("{} not measured: {reason}", id.label()));
                backends.push(BackendStatus {
                    backend: id.label().into(),
                    status: Availability::Unavailable,
                    reason: Some(reason),
                    todo: true,
                    capabilities: None,
                });
            }
        }
    }

    // templates
    let mut templates = Vec::new();
    let mut images = BTreeMap::new();
    if let Some(b) = live.first() {
        for t in &plan.templates {
            let tpl = b.sc.templates().get(t)?;
            let image = tpl.manifest.template.image.clone().unwrap_or_default();
            let mut oci = String::new();
            for lb in &live {
                if let Some(id) = lb.image_id(&image).await {
                    oci = id;
                }
            }
            images.insert(t.clone(), image.clone());
            templates.push(TemplateInfo {
                name: t.clone(),
                image,
                oci_digest: oci,
                // Host-local pin (local integrity, not provenance); empty when unpinned.
                pinned_digest: sandcrab_sdk::core::effective_pin(
                    b.sc.manager().pin_store().as_ref(),
                    &tpl.manifest,
                    sandcrab_sdk::core::ImageArch::host(),
                )?
                .map(|p| p.digest)
                .unwrap_or_default(),
                rootfs_sha256: String::new(),
                vcpu: tpl.manifest.resources.vcpu,
                memory_mb: tpl.manifest.resources.memory_mb,
            });
        }
    }

    // fixtures
    let fx_root = plan.work_dir.join("fixtures");
    let _ = fs::remove_dir_all(&fx_root);
    eprintln!("generating fixtures in {} ...", fx_root.display());
    let fixtures = Fixtures::generate(&fx_root, plan.seed, plan.w2_files, plan.w2_bytes)?;
    let mut w2_names = Vec::new();
    for b in &live {
        let name = format!("bw2-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        b.sc.workspaces()
            .register_host_dir(&name, &fixtures.w2_dir)?;
        w2_names.push(name);
    }

    let ctx = Ctx {
        plan,
        live,
        fixtures,
        w2_names,
        dockerd_pid: sample::pidof("dockerd"),
        can_drop_caches: can_drop_caches(),
        crabd_version: OnceLock::new(),
        images,
    };

    // harness self-overhead
    let own_cgroup = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("0::").map(str::to_owned))
        })
        .map(|rel| PathBuf::from("/sys/fs/cgroup").join(rel.trim_start_matches('/')))
        .unwrap_or_else(|| PathBuf::from("/sys/fs/cgroup"));
    let harness = HarnessOverhead {
        noop_overhead_ns: noop_overhead_ns().await,
        sampler_cpu_pct: tokio::task::block_in_place(|| {
            sample::sampler_cpu_pct(
                &own_cgroup,
                if plan.quick {
                    Duration::from_secs(1)
                } else {
                    Duration::from_secs(5)
                },
            )
        }),
    };

    let mut shared_overhead = BTreeMap::new();
    for (k, name) in [("dockerd_rss", "dockerd"), ("containerd_rss", "containerd")] {
        if let Some(v) = sample::pidof(name).and_then(sample::proc_rss) {
            shared_overhead.insert(k.to_owned(), v);
        }
    }

    let drift_n = plan.runs.min(10);
    let run_drift = plan.scenarios.contains(&Scenario::Drift) && !ctx.live.is_empty();
    let drift_start = if run_drift {
        eprintln!("[drift] start: {drift_n} × spawn_ready base/warm/W0");
        drift_p50(&ctx, drift_n).await
    } else {
        None
    };

    // matrix
    let specs = schedule::shuffled(matrix(plan), plan.seed);
    let mut cells = Vec::new();
    for (i, spec) in specs.iter().enumerate() {
        for st in backends
            .iter()
            .filter(|s| s.status == Availability::Unavailable)
        {
            cells.push(placeholder_cell(
                &st.backend,
                spec,
                CellStatus::Unavailable,
                st.reason.clone().unwrap_or_default(),
                true,
            ));
        }
        if !ctx.live.is_empty() {
            cells.extend(run_cell(&ctx, spec, i).await);
        }
    }
    if plan.caches.contains(&Cache::ColdUncached) || plan.caches.contains(&Cache::Cold) {
        for t in &plan.templates {
            for st in &backends {
                cells.push(placeholder_cell(
                    &st.backend,
                    &CellSpec {
                        template: t.clone(),
                        cache: Cache::ColdUncached,
                        ws: Ws::W0,
                        scenario: Scenario::Lifecycle,
                    },
                    CellStatus::NotMeasured,
                    "cold-uncached needs the template image / rootfs removed from the host \
                     before each rep (docker rmi + rebuild, or a fresh FC rootfs cache); \
                     destructive on a shared host, so not automated in V1"
                        .into(),
                    true,
                ));
            }
        }
    }

    // density
    let mut density = Vec::new();
    if plan.scenarios.contains(&Scenario::Density) {
        let template = plan
            .templates
            .iter()
            .find(|t| *t == "base")
            .or(plan.templates.first())
            .cloned()
            .unwrap_or_else(|| "base".into());
        for bi in 0..ctx.live.len() {
            for touched in [false, true] {
                for rep in 0..plan.density_reps {
                    density.push(density_rep(&ctx, bi, &template, touched, rep).await);
                }
            }
        }
        if plan.density_cap < 500 {
            notes.push(format!(
                "density capped at {} sandboxes (spec: 500) to keep this host safe",
                plan.density_cap
            ));
        }
    }

    let drift_check = match (
        drift_start,
        if run_drift {
            drift_p50(&ctx, drift_n).await
        } else {
            None
        },
    ) {
        (Some(a), Some(z)) => {
            let pct = (z - a) / a * 100.0;
            Some(DriftCheck {
                backend: ctx.live[0].id.label().into(),
                n: drift_n,
                start_p50_ns: a,
                end_p50_ns: z,
                drift_pct: pct,
                flagged: pct.abs() > DRIFT_PCT,
            })
        }
        _ => None,
    };

    for b in &ctx.live {
        let _ = b.sc.shutdown().await;
    }
    host::noise_end(&mut noise);
    host_info.crabd.version = ctx.crabd_version.get().cloned().unwrap_or_default();

    // validity
    if plan.runs < MIN_N || plan.resource_runs < MIN_N {
        invalid.push(format!(
            "n below the spec minimum of {MIN_N} (runs={}, resource_runs={}; --allow-small-n/--quick)",
            plan.runs, plan.resource_runs
        ));
    }
    if noise.load_start[0] > plan.max_load {
        invalid.push(format!(
            "host busy: 1-min load {:.2} > {} at start",
            noise.load_start[0], plan.max_load
        ));
    }
    if let Some(d) = &drift_check
        && d.flagged
    {
        invalid.push(format!(
            "spawn_ready p50 drifted {:.1}% start→end",
            d.drift_pct
        ));
    }
    for c in &cells {
        if c.unreliable {
            invalid.push(format!(
                "cell {} unreliable: {}/{} iterations failed",
                c.key(),
                c.failures.len(),
                c.n
            ));
        }
    }
    if !noise.other_guests.is_empty() {
        notes.push(
            "other Proxmox guests running; per-guest CPU (mpstat 30 s) was not sampled".into(),
        );
    }
    match host_info.kind.as_str() {
        "vm-nested" => notes.push(format!(
            "bench host is a VM ({}) with nested virt: FC is disadvantaged (EPT-on-EPT)",
            host_info.virt
        )),
        "vm" => notes.push(format!(
            "bench host is a VM ({}) without nested virt: Firecracker cannot run here",
            host_info.virt
        )),
        _ => {}
    }
    notes.push(
        "network policy: `disabled` on every backend (egress_restricted is unenforceable in V1)"
            .into(),
    );
    notes.push(
        "docker: bind-mount cost is inside spawn_ready (compare W1/W2 vs W0); ws_attach \
         is the post-ready list_dir verification"
            .into(),
    );
    if plan.backends.contains(&BackendId::Libkrun) {
        notes.push(
            "libkrun: workspaces are live virtio-fs shares (no attach / sync-back cost); without \
             a cgroup leaf (always on macOS) memory is the helper's RSS (mem.helper_rss_*), \
             which counts shared pages fully: not comparable with the Linux PSS columns; idle \
             CPU is the helper process's CPU time (cpu.helper_idle_60s)"
                .into(),
        );
    }
    notes.push(
        "docker: mem.guest_available reads the host /proc/meminfo (no lxcfs), so it is \
         not a per-sandbox number"
            .into(),
    );
    if host_info.governor.starts_with("unavailable") {
        notes.push("cpufreq governor not exposed: could not set `performance`".into());
    }

    let finished = chrono::Utc::now();
    let host_label = plan
        .host_label
        .clone()
        .unwrap_or_else(|| host_info.hostname.clone());
    let result = RunResult {
        schema_version: SCHEMA_VERSION.into(),
        run_id: uuid::Uuid::new_v4().to_string(),
        started_at: started.to_rfc3339(),
        finished_at: finished.to_rfc3339(),
        git: host::git_info(),
        seed: plan.seed,
        host: host_info,
        noise,
        harness,
        templates,
        backends,
        cells,
        density,
        drift_check,
        shared_overhead,
        valid: invalid.is_empty(),
        invalid_reasons: invalid,
        notes,
        config: RunConfig {
            argv: plan.argv.clone(),
            backends: plan.backends.iter().map(|b| b.label().to_owned()).collect(),
            templates: plan.templates.clone(),
            scenarios: plan
                .scenarios
                .iter()
                .map(|s| s.label().to_owned())
                .collect(),
            workspaces: plan
                .workspaces
                .iter()
                .map(|w| w.label().to_owned())
                .collect(),
            caches: plan.caches.iter().map(|c| c.label().to_owned()).collect(),
            runs: plan.runs,
            warmups: plan.warmups,
            calls_per_sandbox: plan.calls,
            call_warmups: plan.call_warmups,
            resource_runs: plan.resource_runs,
            idle_window_s: plan.idle_window.as_secs(),
            settle_s: plan.settle.as_secs(),
            density_budget: plan.density_budget_raw.clone(),
            density_cap: plan.density_cap,
            density_reps: plan.density_reps,
            touch_mib: plan.touch_mib,
            quick: plan.quick,
            allow_small_n: plan.allow_small_n,
            network: "disabled".into(),
        },
    };

    fs::create_dir_all(&plan.out_dir)?;
    let safe_host: String = host_label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let path = plan.out_dir.join(format!(
        "{}-{safe_host}.json",
        started.format("%Y%m%dT%H%M%SZ")
    ));
    fs::write(&path, serde_json::to_string_pretty(&result)? + "\n")?;
    let _ = fs::remove_dir_all(&fx_root);
    Ok((result, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan {
            argv: vec![],
            backends: vec![BackendId::Docker],
            templates: vec!["base".into(), "ts-dev".into()],
            scenarios: Scenario::ALL.into_iter().collect(),
            workspaces: vec![Ws::W0, Ws::W1, Ws::W2],
            caches: vec![Cache::Warm, Cache::Cold],
            runs: 30,
            warmups: 5,
            calls: 30,
            call_warmups: 5,
            resource_runs: 30,
            idle_window: Duration::from_secs(60),
            settle: Duration::from_secs(5),
            density_budget: Budget::PctOfTotal(0.75),
            density_budget_raw: "75%".into(),
            density_cap: 16,
            density_reps: 3,
            density_floor: 1 << 30,
            touch_mib: 64,
            quick: false,
            allow_small_n: false,
            only: None,
            seed: 1,
            out_dir: PathBuf::from("out"),
            host_label: None,
            kind_override: None,
            noise_declared: String::new(),
            max_load: 0.5,
            w2_files: 5000,
            w2_bytes: 50_000_000,
            work_dir: PathBuf::from("/tmp/x"),
            templates_dir: PathBuf::from("templates"),
            crabd: PathBuf::from("crabd"),
        }
    }

    #[test]
    fn matrix_matches_spec() {
        let m = matrix(&plan());
        let lc = |t: &str| {
            m.iter()
                .filter(|c| c.template == t && c.scenario == Scenario::Lifecycle)
                .count()
        };
        // base: {cold, warm} × {W0, W1, W2} = 6 per backend (×2 backends = 12 spec cells)
        assert_eq!(lc("base"), 6);
        // ts-dev: warm × {W0, W2}
        assert_eq!(lc("ts-dev"), 2);
        assert!(
            m.iter()
                .filter(|c| c.template == "ts-dev")
                .all(|c| c.cache == Cache::Warm)
        );
        assert!(
            m.iter()
                .filter(|c| c.scenario == Scenario::AgentTurn)
                .all(|c| c.ws == Ws::W1)
        );
        let mut p = plan();
        p.only = Some("base/warm/W0".into());
        let only = matrix(&p);
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].scenario, Scenario::Lifecycle);
    }

    #[test]
    fn budget_parsing() {
        assert_eq!(Budget::parse("75%").unwrap(), Budget::PctOfTotal(0.75));
        assert_eq!(Budget::parse("2GiB").unwrap(), Budget::Bytes(2 << 30));
        assert_eq!(Budget::parse("512M").unwrap(), Budget::Bytes(512 << 20));
        assert!(Budget::parse("150%").is_err());
        // 75% of 8 GiB minus 2 GiB already used = 4 GiB
        let b = Budget::PctOfTotal(0.75).bytes(8 << 30, 6 << 30);
        assert_eq!(b, 4 << 30);
    }

    #[test]
    fn scenario_and_ws_parsing() {
        assert_eq!(Scenario::parse("all").unwrap().len(), 6);
        assert!(Scenario::parse("bogus").is_err());
        assert_eq!(Ws::parse("w2").unwrap(), Ws::W2);
        assert_eq!(Cache::parse("cold-uncached").unwrap(), Cache::ColdUncached);
    }

    #[test]
    fn acc_excludes_failed_metric_and_flags_unreliable() {
        let mut acc = Acc::default();
        let mut ok = IterOut::default();
        ok.ns("spawn_ready", Duration::from_millis(5));
        acc.add(0, ok);
        let mut bad = IterOut::default();
        bad.ns("spawn_ready", Duration::from_millis(6));
        bad.fail("first_call", "boom");
        acc.add(1, bad);
        assert_eq!(acc.metrics["spawn_ready"].1.len(), 2);
        assert!(!acc.metrics.contains_key("first_call"));
        assert_eq!(acc.failures.len(), 1);
        assert_eq!(acc.failures[0].iter, 1);
    }
}
