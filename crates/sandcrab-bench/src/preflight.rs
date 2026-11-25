//! Preflight: predict, in about a minute, whether a run at a given tier would come out valid,
//! before spending hours on it (`sandcrab-bench preflight`, and the start of `run`).
//!
//! What it can predict: a host that is busy now (the start-load rule), storage known to drift
//! (HDD under Docker), a density budget that is already exhausted, a disk that will fill, a
//! backend that will not open or cannot spawn, and short-term spawn instability (a background
//! job in progress, caches still warming).
//!
//! What it cannot predict: slow drift over hours (page cache growth, an HDD's caches settling,
//! a cron job or a desktop that wakes up later). The drift probe compares two small groups of
//! `spawn_ready` samples a short window apart; a clean probe is necessary, not sufficient.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use sandcrab_sdk::{NetworkPolicy, SandboxConfig};

use crate::backend::LiveBackend;
use crate::host;
use crate::results::{
    Availability, BackendStatus, DriftProbe, Finding, Preflight, PreflightStorage, Severity,
};
use crate::run::{Budget, DRIFT_PCT};
use crate::stats;
use crate::validity::Tier;

/// Below this much free disk a run is refused at any tier.
pub const DISK_FAIL_BYTES: u64 = 5 << 30;
/// Below this much free disk, warn (AGENTS.md: clean up below ~15 GB).
pub const DISK_WARN_BYTES: u64 = 15 << 30;
/// Probe spread above which the probe warns that spawn latency is noisy.
pub const PROBE_CV_WARN_PCT: f64 = 25.0;
/// Probe drift above which it warns (half the run's threshold).
pub const PROBE_DRIFT_WARN_PCT: f64 = DRIFT_PCT / 2.0;

/// What the preflight checks against.
#[derive(Debug, Clone)]
pub struct Opts {
    pub tier: Tier,
    pub max_load: f64,
    /// The density scenario is requested.
    pub density: bool,
    pub density_budget: Budget,
    pub density_budget_raw: String,
    pub density_cap: usize,
    pub density_floor: u64,
    /// Run the drift probe.
    pub probe: bool,
    pub probe_samples: usize,
    pub probe_window: Duration,
    /// Template the probe spawns (the run's drift check uses the first template).
    pub template: String,
    pub work_dir: PathBuf,
    pub docker_requested: bool,
}

struct Ctx {
    tier: Tier,
    findings: Vec<Finding>,
}

impl Ctx {
    /// `fail` only for a tier where the condition invalidates the run (Standard/Full);
    /// a Smoke run is never valid anyway, so for Smoke it is a warning.
    fn tiered(&mut self, check: &str, message: String, fix: &str) {
        let sev = if self.tier == Tier::Smoke {
            Severity::Warn
        } else {
            Severity::Fail
        };
        self.push(check, sev, message, Some(fix));
    }

    fn push(&mut self, check: &str, severity: Severity, message: String, fix: Option<&str>) {
        self.findings.push(Finding {
            check: check.into(),
            severity,
            message,
            fix: fix.map(str::to_owned),
        });
    }
}

fn gib(b: u64) -> f64 {
    b as f64 / (1u64 << 30) as f64
}

/// Spawn and destroy one sandbox; `spawn_ready` in ns (the same window as the run's).
async fn spawn_once(b: &LiveBackend, template: &str) -> Result<f64, String> {
    let cfg = SandboxConfig::from_template(template).network(NetworkPolicy::Disabled);
    let t0 = Instant::now();
    let sb = b.sc.spawn(cfg).await.map_err(|e| e.to_string())?;
    let ns = t0.elapsed().as_nanos() as f64;
    sb.destroy().await.map_err(|e| e.to_string())?;
    Ok(ns)
}

async fn samples(b: &LiveBackend, template: &str, n: usize) -> Result<Vec<f64>, String> {
    let mut xs = Vec::with_capacity(n);
    for _ in 0..n {
        xs.push(spawn_once(b, template).await?);
    }
    Ok(xs)
}

fn p50(xs: &[f64]) -> Option<f64> {
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    (!v.is_empty()).then(|| stats::percentile(&v, 0.5))
}

/// Coefficient of variation, %.
fn cv_pct(xs: &[f64]) -> Option<f64> {
    if xs.len() < 2 {
        return None;
    }
    let mean = xs.iter().sum::<f64>() / xs.len() as f64;
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (xs.len() - 1) as f64;
    (mean > 0.0).then(|| var.sqrt() / mean * 100.0)
}

/// The two groups do not overlap at all: with 5 + 5 samples that happens by chance with
/// probability 2/252 (< 1%), so a separated shift is a real one, not sampling noise.
fn separated(a: &[f64], z: &[f64]) -> bool {
    let max = |v: &[f64]| v.iter().copied().fold(f64::MIN, f64::max);
    let min = |v: &[f64]| v.iter().copied().fold(f64::MAX, f64::min);
    !a.is_empty() && !z.is_empty() && (min(z) > max(a) || max(z) < min(a))
}

/// Drift probe: one discarded warmup spawn, `n` timed spawns per backend, an idle window,
/// then `n` more. Same metric as the run's drift check (`spawn_ready` p50).
async fn drift_probe(o: &Opts, live: &[LiveBackend]) -> Vec<DriftProbe> {
    let mut probes: Vec<DriftProbe> = Vec::new();
    for b in live {
        eprintln!(
            "[preflight] drift probe {}: {} spawns",
            b.id.label(),
            o.probe_samples
        );
        let start = match spawn_once(b, &o.template).await {
            Ok(_) => samples(b, &o.template, o.probe_samples).await,
            Err(e) => Err(e),
        };
        let (start_ns, error) = match start {
            Ok(v) => (v, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        probes.push(DriftProbe {
            backend: b.id.label().into(),
            template: o.template.clone(),
            start_p50_ns: p50(&start_ns),
            start_ns,
            end_ns: Vec::new(),
            window_s: o.probe_window.as_secs(),
            end_p50_ns: None,
            drift_pct: None,
            cv_pct: None,
            error,
        });
    }
    if probes.iter().any(|p| p.error.is_none()) {
        eprintln!(
            "[preflight] drift probe: idle {} s",
            o.probe_window.as_secs()
        );
        tokio::time::sleep(o.probe_window).await;
    }
    for (b, p) in live.iter().zip(probes.iter_mut()) {
        if p.error.is_some() {
            continue;
        }
        match samples(b, &o.template, o.probe_samples).await {
            Ok(v) => {
                p.end_p50_ns = p50(&v);
                p.end_ns = v;
                let all: Vec<f64> = p.start_ns.iter().chain(&p.end_ns).copied().collect();
                p.cv_pct = cv_pct(&all);
                if let (Some(a), Some(z)) = (p.start_p50_ns, p.end_p50_ns) {
                    p.drift_pct = Some((z - a) / a * 100.0);
                }
            }
            Err(e) => p.error = Some(e),
        }
    }
    probes
}

/// Run every check. `load` is the host load sampled before any of the bench's own activity
/// (for `run`: the run's recorded start load), `live`/`backends` the opened backends.
pub async fn check(
    o: &Opts,
    load: [f64; 3],
    live: &[LiveBackend],
    backends: &[BackendStatus],
) -> Preflight {
    let t0 = Instant::now();
    let at = chrono::Utc::now().to_rfc3339();
    let mut c = Ctx {
        tier: o.tier,
        findings: Vec::new(),
    };

    // load: the run's validity rule is the 1-min value at start; 5/15-min show a recent past.
    if load[0] > o.max_load {
        c.tiered(
            "load",
            format!(
                "1-min load {:.2} > {} now: the run would be marked invalid (host busy at start)",
                load[0], o.max_load
            ),
            "close apps (browsers, IDEs, builds, VMs), stop other workloads, and wait until \
             `uptime` shows the 1-min load under the limit",
        );
    } else if load[1] > o.max_load || load[2] > o.max_load {
        c.push(
            "load",
            Severity::Warn,
            format!(
                "5/15-min load {:.2}/{:.2} > {}: the host was busy recently and may be again \
                 (only the 1-min value at start is checked)",
                load[1], load[2], o.max_load
            ),
            Some("wait for the 5-min load to settle too"),
        );
    }

    // memory and the density budget
    let mem = host::meminfo();
    let total = mem.get("MemTotal").copied().unwrap_or(0);
    let avail = mem.get("MemAvailable").copied().unwrap_or(0);
    if avail < o.density_floor {
        c.tiered(
            "memory",
            format!(
                "only {:.1} GiB available (floor {:.1} GiB): spawns may fail and density stops \
                 at once on the safety floor",
                gib(avail),
                gib(o.density_floor)
            ),
            "close apps to free memory",
        );
    }
    let sandbox_bytes = live.first().and_then(|b| {
        b.sc.templates()
            .get(&o.template)
            .ok()
            .map(|t| t.manifest.resources.memory_mb << 20)
    });
    let budget_bytes = o.density.then(|| o.density_budget.bytes(total, avail));
    if let Some(budget) = budget_bytes {
        let per = sandbox_bytes.unwrap_or(512 << 20).max(1);
        let fits = budget / per;
        if fits < 2 {
            c.tiered(
                "density",
                format!(
                    "density budget {} leaves {} MiB ({:.1} GiB of {:.1} GiB in use): with \
                     sandboxes up to {} MiB, density would stop after {} and measure this \
                     host's free memory, not the backend",
                    o.density_budget_raw,
                    budget >> 20,
                    gib(total.saturating_sub(avail)),
                    gib(total),
                    per >> 20,
                    fits.max(1)
                ),
                "close apps to free memory, or pass an absolute --density-budget that fits in \
                 MemAvailable (e.g. --density-budget 2GiB), or leave density out \
                 (--scenarios without `density`)",
            );
        } else if (fits as usize) < o.density_cap {
            c.push(
                "density",
                Severity::Warn,
                format!(
                    "density budget {} MiB fits ~{fits} sandboxes of up to {} MiB, below \
                     --density-cap {}: max_n will likely be budget-bound",
                    budget >> 20,
                    per >> 20,
                    o.density_cap
                ),
                None,
            );
        }
    }

    // storage + free disk, per place the run writes to
    let mut places = vec![("work-dir", o.work_dir.clone())];
    if let Some(h) = host::sandcrab_home().filter(|p| p.exists()) {
        places.push(("sandcrab-home", h));
    }
    if o.docker_requested
        && let Some(r) = host::docker_root()
    {
        places.push(("docker-root", r));
    }
    let docker_live = live
        .iter()
        .any(|b| b.id == crate::backend::BackendId::Docker);
    let mut storage: Vec<PreflightStorage> = Vec::new();
    for (role, path) in places {
        let st = host::storage_for(&path);
        let free = host::disk_free(&path);
        let entry = PreflightStorage {
            role: role.into(),
            device: st.device.clone(),
            kind: st.kind.clone(),
            mount: free
                .as_ref()
                .map_or_else(|| st.probed_path.clone(), |f| f.1.clone()),
            free_bytes: free.as_ref().map(|f| f.0),
        };
        // One disk finding per filesystem, however many roles share it.
        let first_on_mount = !storage.iter().any(|s| s.mount == entry.mount);
        if let Some(f) = entry.free_bytes.filter(|_| first_on_mount) {
            if f < DISK_FAIL_BYTES {
                c.push(
                    "disk",
                    Severity::Fail,
                    format!(
                        "{role}: only {:.1} GiB free on {}: rootfs images, fixtures and \
                         density sandboxes can fill it mid-run",
                        gib(f),
                        entry.mount
                    ),
                    Some(
                        "free space first (cargo clean -p, docker builder prune -f, \
                         docker image prune -f)",
                    ),
                );
            } else if f < DISK_WARN_BYTES {
                c.push(
                    "disk",
                    Severity::Warn,
                    format!("{role}: {:.1} GiB free on {}", gib(f), entry.mount),
                    Some("free space before a long run"),
                );
            }
        }
        if st.kind == "hdd" {
            if role == "docker-root" && docker_live && o.tier == Tier::Full {
                c.push(
                    "storage",
                    Severity::Fail,
                    format!(
                        "Docker's root is on an HDD ({}): Docker spawn/teardown are disk-bound \
                         and drift over a multi-hour run (kvm-dedicated: -11.9% over 3 h, \
                         invalid); a short probe cannot rule it out",
                        st.device
                    ),
                    Some(
                        "use an SSD/NVMe host for a Full run with Docker, run the microVM \
                         backends without Docker, or --force to accept the drift risk",
                    ),
                );
            } else if !storage.iter().any(|s| s.device == st.device) {
                c.push(
                    "storage",
                    Severity::Warn,
                    format!(
                        "{role} is on an HDD ({}): disk-bound timings, drift risk over long runs",
                        st.device
                    ),
                    Some("prefer an SSD/NVMe host"),
                );
            }
        }
        storage.push(entry);
    }

    // backends
    for b in backends
        .iter()
        .filter(|b| b.status == Availability::Unavailable)
    {
        c.push(
            "backends",
            Severity::Warn,
            format!(
                "{} unavailable: {} (recorded as unavailable, never substituted)",
                b.backend,
                b.reason.as_deref().unwrap_or("")
            ),
            None,
        );
    }
    if live.is_empty() {
        c.push(
            "backends",
            Severity::Fail,
            "no requested backend is available: nothing would be measured".into(),
            Some("see the reasons above (`sandcrab doctor`)"),
        );
    }

    // drift probe
    let drift_probe = if o.probe && !live.is_empty() {
        drift_probe(o, live).await
    } else {
        Vec::new()
    };
    for (i, p) in drift_probe.iter().enumerate() {
        // The run's drift check measures the first available backend; others only warn.
        let gating = i == 0;
        if let Some(e) = &p.error {
            c.push(
                "drift",
                Severity::Fail,
                format!("{}: probe spawn failed: {e}", p.backend),
                Some("fix the backend first (`sandcrab doctor`); every cell would fail"),
            );
            continue;
        }
        let Some(pct) = p.drift_pct else { continue };
        let sep = separated(&p.start_ns, &p.end_ns);
        if pct.abs() > DRIFT_PCT && sep && gating {
            c.tiered(
                "drift",
                format!(
                    "{}: spawn_ready p50 moved {pct:+.1}% within {} s (all samples shifted): \
                     the run's drift check (> {DRIFT_PCT}%) would very likely fail",
                    p.backend, p.window_s
                ),
                "find what is changing on the host (top, iostat), let caches warm up \
                 (run the preflight again), then retry",
            );
        } else if pct.abs() > PROBE_DRIFT_WARN_PCT {
            c.push(
                "drift",
                Severity::Warn,
                format!(
                    "{}: spawn_ready p50 moved {pct:+.1}% within {} s{}",
                    p.backend,
                    p.window_s,
                    if sep {
                        ""
                    } else {
                        " (groups overlap: may be noise)"
                    }
                ),
                Some("run the preflight again; if it persists, find what is changing"),
            );
        }
        if let Some(cv) = p.cv_pct
            && cv > PROBE_CV_WARN_PCT
        {
            c.push(
                "drift",
                Severity::Warn,
                format!(
                    "{}: spawn_ready varies a lot (cv {cv:.0}%): noisy host or backend",
                    p.backend
                ),
                None,
            );
        }
    }

    let would_be_valid = !c.findings.iter().any(|f| f.severity == Severity::Fail);
    Preflight {
        tier: o.tier,
        at,
        duration_s: t0.elapsed().as_secs_f64(),
        load,
        max_load: o.max_load,
        mem_total_bytes: total,
        mem_available_bytes: avail,
        density_budget: o.density.then(|| o.density_budget_raw.clone()),
        density_budget_bytes: budget_bytes,
        density_sandbox_bytes: sandbox_bytes,
        storage,
        backends: backends.to_vec(),
        drift_probe,
        findings: c.findings,
        would_be_valid,
        forced: false,
    }
}

/// The verdict line(s) only.
#[must_use]
pub fn verdict(p: &Preflight) -> String {
    let fails: Vec<&Finding> = p
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Fail)
        .collect();
    let mut s = String::new();
    if p.tier == Tier::Smoke {
        let _ = write!(
            s,
            "verdict: smoke runs check correctness only and are never valid for publishing; "
        );
        if fails.is_empty() {
            s.push_str("nothing here would break the run");
        } else {
            let _ = write!(s, "the run may break because: ");
            s.push_str(
                &fails
                    .iter()
                    .map(|f| f.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
    } else if fails.is_empty() {
        let _ = write!(
            s,
            "verdict: would be valid for {} (as far as a preflight can tell: it cannot see \
             drift that builds up over hours)",
            p.tier.label()
        );
    } else {
        let _ = writeln!(
            s,
            "verdict: will be marked invalid for {} because:",
            p.tier.label()
        );
        for f in &fails {
            let _ = writeln!(s, "  - {}", f.message);
            if let Some(fix) = &f.fix {
                let _ = writeln!(s, "    fix: {fix}");
            }
        }
        s.truncate(s.trim_end().len());
    }
    s
}

/// Human-readable report.
#[must_use]
pub fn render(p: &Preflight) -> String {
    let mut s = String::new();
    let mut line = |k: &str, v: String| {
        let _ = writeln!(s, "  {k:<12} {v}");
    };
    line(
        "load",
        format!(
            "{:.2} {:.2} {:.2} (limit {} on the 1-min value at start)",
            p.load[0], p.load[1], p.load[2], p.max_load
        ),
    );
    line(
        "memory",
        format!(
            "{:.1} GiB available of {:.1} GiB",
            gib(p.mem_available_bytes),
            gib(p.mem_total_bytes)
        ),
    );
    if let (Some(raw), Some(b)) = (&p.density_budget, p.density_budget_bytes) {
        line(
            "density",
            format!(
                "budget {raw} → {} MiB{}",
                b >> 20,
                p.density_sandbox_bytes
                    .map(|x| format!(" (sandbox ≤ {} MiB)", x >> 20))
                    .unwrap_or_default()
            ),
        );
    }
    for st in &p.storage {
        line(
            "storage",
            format!(
                "{}: {} {}{}",
                st.role,
                st.device,
                st.kind,
                st.free_bytes
                    .map(|f| format!(", {:.1} GiB free on {}", gib(f), st.mount))
                    .unwrap_or_default()
            ),
        );
    }
    for b in &p.backends {
        line(
            "backend",
            match b.status {
                Availability::Available => format!("{}: available", b.backend),
                Availability::Unavailable => format!(
                    "{}: unavailable — {}",
                    b.backend,
                    b.reason.as_deref().unwrap_or("")
                ),
            },
        );
    }
    for d in &p.drift_probe {
        line(
            "drift probe",
            match (&d.error, d.start_p50_ns, d.end_p50_ns, d.drift_pct) {
                (Some(e), ..) => format!("{} {}: failed: {e}", d.backend, d.template),
                (None, Some(a), Some(z), Some(pct)) => format!(
                    "{} {}: {}+{} spawns {} s apart, p50 {:.1} → {:.1} ms ({pct:+.1}%), cv {:.0}%",
                    d.backend,
                    d.template,
                    d.start_ns.len(),
                    d.end_ns.len(),
                    d.window_s,
                    a / 1e6,
                    z / 1e6,
                    d.cv_pct.unwrap_or(0.0)
                ),
                _ => format!("{} {}: no samples", d.backend, d.template),
            },
        );
    }
    let mut out = format!(
        "preflight for a {} run ({:.0} s)\n{s}",
        p.tier.label(),
        p.duration_s
    );
    if !p.findings.is_empty() {
        out.push_str("findings:\n");
        for f in &p.findings {
            let _ = writeln!(
                out,
                "  {} {}: {}",
                match f.severity {
                    Severity::Fail => "FAIL",
                    Severity::Warn => "warn",
                },
                f.check,
                f.message
            );
            if let Some(fix) = &f.fix {
                let _ = writeln!(out, "       fix: {fix}");
            }
        }
    }
    out.push_str(&verdict(p));
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separation_and_cv() {
        assert!(separated(&[1.0, 2.0, 3.0], &[4.0, 5.0]));
        assert!(separated(&[4.0, 5.0], &[1.0, 2.0]));
        assert!(!separated(&[1.0, 4.0], &[2.0, 5.0]));
        assert!(!separated(&[], &[1.0]));
        assert!(cv_pct(&[1.0]).is_none());
        assert!(cv_pct(&[10.0, 10.0, 10.0]).unwrap().abs() < 1e-9);
        assert_eq!(p50(&[3.0, 1.0, 2.0]), Some(2.0));
    }

    fn pf(tier: Tier, findings: Vec<Finding>) -> Preflight {
        Preflight {
            tier,
            at: String::new(),
            duration_s: 1.0,
            load: [0.1; 3],
            max_load: 0.5,
            mem_total_bytes: 16 << 30,
            mem_available_bytes: 8 << 30,
            density_budget: None,
            density_budget_bytes: None,
            density_sandbox_bytes: None,
            storage: vec![],
            backends: vec![],
            drift_probe: vec![],
            would_be_valid: !findings.iter().any(|f| f.severity == Severity::Fail),
            findings,
            forced: false,
        }
    }

    #[test]
    fn verdict_names_reason_and_fix() {
        let f = Finding {
            check: "load".into(),
            severity: Severity::Fail,
            message: "1-min load 4.00 > 0.5 now".into(),
            fix: Some("close apps".into()),
        };
        let v = verdict(&pf(Tier::Full, vec![f.clone()]));
        assert!(v.starts_with("verdict: will be marked invalid for full because:"));
        assert!(v.contains("1-min load 4.00") && v.contains("fix: close apps"));
        assert!(
            verdict(&pf(Tier::Standard, vec![]))
                .starts_with("verdict: would be valid for standard")
        );
        assert!(verdict(&pf(Tier::Smoke, vec![])).contains("never valid for publishing"));
    }
}
