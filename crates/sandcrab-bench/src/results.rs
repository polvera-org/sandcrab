//! Results file model (bench-spec §6). One JSON file per run; `schema_version` "1".
//!
//! Fields beyond the spec's sketch are additive (`status`/`reason`/`todo` on cells and
//! metrics, `backends`, `notes`, `config`, `shared_overhead`) so a backend or cell that could
//! not be measured is recorded explicitly instead of silently missing. The JSON Schema is
//! generated from these types (`sandcrab-bench schema`) and committed at
//! `docs/bench/results/schema.json`; a unit test keeps the two in sync.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::stats::Stats;

pub const SCHEMA_VERSION: &str = "1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunResult {
    pub schema_version: String,
    pub run_id: String,
    /// RFC 3339, UTC.
    pub started_at: String,
    pub finished_at: String,
    pub git: GitInfo,
    pub seed: u64,
    pub host: HostInfo,
    pub noise: Noise,
    pub harness: HarnessOverhead,
    pub templates: Vec<TemplateInfo>,
    /// Availability of every requested backend.
    pub backends: Vec<BackendStatus>,
    pub cells: Vec<Cell>,
    pub density: Vec<DensityRun>,
    pub drift_check: Option<DriftCheck>,
    /// Shared, amortised daemon memory (dockerd / containerd), reported once per run.
    pub shared_overhead: BTreeMap<String, u64>,
    pub valid: bool,
    pub invalid_reasons: Vec<String>,
    /// Deviations from the spec and other context for the report.
    pub notes: Vec<String>,
    pub config: RunConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitInfo {
    pub commit: String,
    pub dirty: bool,
    pub branch: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HostInfo {
    pub hostname: String,
    /// `bare-metal`, `vm` (no nested virt exposed) or `vm-nested`.
    pub kind: String,
    /// `systemd-detect-virt` output (`none` on bare metal).
    pub virt: String,
    pub kind_overridden: bool,
    pub hypervisor: Option<String>,
    pub cpu_model: String,
    pub sockets: u32,
    pub cores: u32,
    pub threads: u32,
    pub smt: bool,
    pub kernel: String,
    pub governor: String,
    /// `on` | `off` | `unknown`.
    pub turbo: String,
    pub mem_total_bytes: u64,
    pub swap_bytes: u64,
    pub thp: String,
    pub ksm: Ksm,
    /// `/dev/kvm` usable by this process.
    pub kvm_available: bool,
    /// VT-x/AMD-V exposed to this kernel (needed for Firecracker inside a VM).
    pub nested_virt: bool,
    pub storage: Storage,
    pub docker: DockerInfo,
    pub firecracker: FirecrackerInfo,
    pub crabd: CrabdInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Ksm {
    pub run: i64,
    pub pages_sharing: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Storage {
    pub device: String,
    /// `nvme` | `ssd` | `hdd` | `unknown`.
    #[serde(rename = "type")]
    pub kind: String,
    pub fs: String,
    pub reflink: bool,
    /// Directory the storage was probed for (Docker root dir).
    pub probed_path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DockerInfo {
    pub engine: String,
    pub containerd: String,
    pub runc: String,
    pub cgroup: String,
    pub storage_driver: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FirecrackerInfo {
    pub version: String,
    pub jailer: bool,
    pub guest_kernel: String,
    pub guest_kernel_sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CrabdInfo {
    pub path: String,
    pub sha256: String,
    /// From the first `hello` of the run.
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Noise {
    pub load_start: [f64; 3],
    pub load_end: [f64; 3],
    pub other_guests: Vec<String>,
    pub declared: String,
    /// Top 10 processes by CPU at start.
    pub top_cpu: Vec<String>,
    pub top_cpu_end: Vec<String>,
    /// Swap in use at start (bytes).
    pub swap_used_start: u64,
    /// The 1-minute load at start exceeded `max_load`.
    pub noisy: bool,
    pub max_load: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessOverhead {
    /// Median cost of timing an empty future.
    pub noop_overhead_ns: f64,
    /// CPU used by the 10 Hz cgroup sampler, % of one core.
    pub sampler_cpu_pct: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TemplateInfo {
    pub name: String,
    pub image: String,
    /// Image ID actually present on this host.
    pub oci_digest: String,
    /// Pin in `sandcrab.toml` (empty if none).
    pub pinned_digest: String,
    pub rootfs_sha256: String,
    pub vcpu: u32,
    pub memory_mb: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackendStatus {
    pub backend: String,
    pub status: Availability,
    pub reason: Option<String>,
    /// Measuring this backend is still to do (e.g. needs a KVM host).
    pub todo: bool,
    pub capabilities: Option<Capabilities>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Capabilities {
    pub hardware_isolated: bool,
    pub multi_tenant_safe: bool,
    pub supports_snapshots: bool,
    pub supports_host_dir_mounts: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CellStatus {
    Ok,
    Unavailable,
    NotMeasured,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Cell {
    pub backend: String,
    pub capabilities: Option<Capabilities>,
    pub template: String,
    /// `cold` | `warm` | `cold-uncached`.
    pub cache: String,
    /// `W0` | `W1` | `W2`.
    pub workspace: String,
    /// `lifecycle` | `agent-turn` | `resources`.
    pub scenario: String,
    pub status: CellStatus,
    pub reason: Option<String>,
    pub todo: bool,
    pub warmup: usize,
    pub n: usize,
    pub failures: Vec<Failure>,
    /// Failure rate > 1%.
    pub unreliable: bool,
    pub metrics: BTreeMap<String, Metric>,
    /// Backend-reported spawn/teardown marks, as offsets (ns) from the spawn request.
    pub phases: BTreeMap<String, Phase>,
}

impl Cell {
    #[must_use]
    pub fn key(&self) -> String {
        format!(
            "{}/{}/{}/{}/{}",
            self.backend, self.template, self.cache, self.workspace, self.scenario
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Failure {
    pub iter: usize,
    pub metric: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Ns,
    Bytes,
    CpuS,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Metric {
    pub unit: Unit,
    pub samples: Vec<f64>,
    pub stats: Option<Stats>,
    /// Set when the metric could not be measured on this backend/host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_measured: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Phase {
    pub samples: Vec<f64>,
    pub stats: Option<Stats>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DensityRun {
    pub backend: String,
    pub template: String,
    /// `idle` | `touched`.
    pub variant: String,
    pub rep: usize,
    pub budget_bytes: u64,
    /// Safety cap on the sandbox count (`--density-cap`).
    pub cap: usize,
    pub max_n: usize,
    /// `budget` | `failure` | `cap` | `floor` (host `MemAvailable` safety floor).
    pub stop_reason: String,
    pub error: Option<String>,
    pub steps: Vec<DensityStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DensityStep {
    pub i: usize,
    pub spawn_ready_ns: u64,
    pub host_mem_available: u64,
    /// Sum of per-sandbox PSS (None when `/proc/<pid>/smaps_rollup` is unreadable).
    pub host_pss_total: Option<u64>,
    /// Sum of the sandbox cgroups' `memory.current`.
    pub cgroup_mem_total: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DriftCheck {
    pub backend: String,
    pub n: usize,
    pub start_p50_ns: f64,
    pub end_p50_ns: f64,
    pub drift_pct: f64,
    pub flagged: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunConfig {
    pub argv: Vec<String>,
    pub backends: Vec<String>,
    pub templates: Vec<String>,
    pub scenarios: Vec<String>,
    pub workspaces: Vec<String>,
    pub caches: Vec<String>,
    pub runs: usize,
    pub warmups: usize,
    pub calls_per_sandbox: usize,
    pub call_warmups: usize,
    pub resource_runs: usize,
    pub idle_window_s: u64,
    pub settle_s: u64,
    pub density_budget: String,
    pub density_cap: usize,
    pub density_reps: usize,
    pub touch_mib: u64,
    pub quick: bool,
    pub allow_small_n: bool,
    pub network: String,
}

/// JSON Schema of [`RunResult`], pretty-printed with a trailing newline.
#[must_use]
pub fn schema_json() -> String {
    let schema = schemars::schema_for!(RunResult);
    let mut s = serde_json::to_string_pretty(&schema).expect("schema serializes");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats;

    fn sample() -> RunResult {
        let mut metrics = BTreeMap::new();
        let xs = vec![1.0e6, 2.0e6, 3.0e6];
        metrics.insert(
            "spawn_ready".to_owned(),
            Metric {
                unit: Unit::Ns,
                stats: stats::compute(&xs, 1),
                samples: xs,
                not_measured: None,
            },
        );
        metrics.insert(
            "mem.vmm_overhead".to_owned(),
            Metric {
                unit: Unit::Bytes,
                samples: vec![],
                stats: None,
                not_measured: Some("firecracker only".into()),
            },
        );
        RunResult {
            schema_version: SCHEMA_VERSION.into(),
            run_id: "00000000-0000-0000-0000-000000000000".into(),
            started_at: "2026-11-23T00:00:00Z".into(),
            finished_at: "2026-11-23T00:01:00Z".into(),
            git: GitInfo::default(),
            seed: 7,
            host: HostInfo::default(),
            noise: Noise::default(),
            harness: HarnessOverhead::default(),
            templates: vec![TemplateInfo::default()],
            backends: vec![BackendStatus {
                backend: "firecracker".into(),
                status: Availability::Unavailable,
                reason: Some("/dev/kvm missing".into()),
                todo: true,
                capabilities: None,
            }],
            cells: vec![Cell {
                backend: "docker-dev".into(),
                capabilities: Some(Capabilities::default()),
                template: "base".into(),
                cache: "warm".into(),
                workspace: "W0".into(),
                scenario: "lifecycle".into(),
                status: CellStatus::Ok,
                reason: None,
                todo: false,
                warmup: 5,
                n: 3,
                failures: vec![Failure {
                    iter: 1,
                    metric: "first_call".into(),
                    error: "boom".into(),
                }],
                unreliable: true,
                metrics,
                phases: BTreeMap::new(),
            }],
            density: vec![DensityRun {
                backend: "docker-dev".into(),
                template: "base".into(),
                variant: "idle".into(),
                rep: 0,
                budget_bytes: 1,
                cap: 2,
                max_n: 2,
                stop_reason: "cap".into(),
                error: None,
                steps: vec![DensityStep {
                    i: 0,
                    spawn_ready_ns: 5,
                    host_mem_available: 6,
                    host_pss_total: None,
                    cgroup_mem_total: Some(7),
                }],
            }],
            drift_check: None,
            shared_overhead: BTreeMap::from([("dockerd_rss".to_owned(), 1)]),
            valid: false,
            invalid_reasons: vec!["n < 30".into()],
            notes: vec![],
            config: RunConfig::default(),
        }
    }

    #[test]
    fn json_round_trip() {
        let r = sample();
        let json = serde_json::to_string_pretty(&r).unwrap();
        let back: RunResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        // spec field names on the wire
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema_version"], "1");
        assert_eq!(v["cells"][0]["metrics"]["spawn_ready"]["unit"], "ns");
        assert_eq!(v["cells"][0]["status"], "ok");
        assert_eq!(v["backends"][0]["status"], "unavailable");
        assert_eq!(v["backends"][0]["todo"], true);
        assert!(v["cells"][0]["metrics"]["spawn_ready"]["stats"]["p50_ci95"].is_array());
        assert!(v["host"]["storage"]["type"].is_string());
    }

    #[test]
    fn committed_schema_is_current() {
        let committed = include_str!("../../../docs/bench/results/schema.json");
        assert_eq!(
            committed,
            schema_json(),
            "docs/bench/results/schema.json is stale: run \
             `cargo run -p sandcrab-bench -- schema > docs/bench/results/schema.json`"
        );
    }

    #[test]
    fn schema_requires_spec_fields() {
        let schema: serde_json::Value = serde_json::from_str(&schema_json()).unwrap();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for f in [
            "schema_version",
            "run_id",
            "git",
            "seed",
            "host",
            "noise",
            "harness",
            "templates",
            "cells",
            "density",
            "valid",
            "invalid_reasons",
        ] {
            assert!(required.contains(&f), "{f} not required");
        }
    }
}
