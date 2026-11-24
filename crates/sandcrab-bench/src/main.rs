//! `sandcrab-bench`: backend-generic benchmark runner for Sandcrab
//! (`docs/plans/phase-1-mvp.bench-spec.md`). See `docs/bench/README.md`.

mod backend;
mod fixtures;
mod host;
mod results;
mod rng;
mod run;
mod sample;
mod schedule;
mod stats;
mod summarize;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::{Args, Parser, Subcommand};

use crate::backend::{BackendId, OpenOpts};
use crate::run::{Budget, Cache, Plan, Scenario, Ws};

#[derive(Debug, Parser)]
#[command(
    name = "sandcrab-bench",
    version,
    about = "Benchmark Sandcrab runtime backends (bench-spec: docs/plans/phase-1-mvp.bench-spec.md)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the benchmark matrix and write a results JSON.
    Run(Box<RunArgs>),
    /// Print the host declaration block (CPU, memory, virt, storage, Docker, KSM, load).
    HostInfo {
        /// Override the detected host kind (bare-metal | vm | vm-nested).
        #[arg(long)]
        host_kind: Option<String>,
        /// Static crabd to fingerprint.
        #[arg(long, env = "SANDCRAB_CRABD")]
        crabd: Option<PathBuf>,
        /// Print JSON instead of the text block.
        #[arg(long)]
        json: bool,
    },
    /// Print a p50/p95/p99/min/max/stddev table for a results file.
    Summarize { file: PathBuf },
    /// Print the results JSON Schema (committed at docs/bench/results/schema.json).
    Schema,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Backends, interleaved ABAB: docker[,firecracker][,libkrun]. Unavailable ones are recorded, not
    /// substituted.
    #[arg(long, value_delimiter = ',', default_value = "docker")]
    backends: Vec<String>,
    /// Templates: base[,ts-dev]. Non-base templates run warm × {W0, W2} only (spec §3).
    #[arg(long, value_delimiter = ',', default_value = "base")]
    templates: Vec<String>,
    /// lifecycle,calls,agent-turn,resources,density,drift or `all`.
    #[arg(long, value_delimiter = ',', default_value = "all")]
    scenarios: Vec<String>,
    /// Workspace variants for lifecycle cells.
    #[arg(long, value_delimiter = ',', default_value = "W0,W1,W2")]
    workspaces: Vec<String>,
    /// Cache modes: warm, cold (needs root), cold-uncached (recorded as not measured).
    #[arg(long, value_delimiter = ',', default_value = "warm,cold")]
    caches: Vec<String>,
    /// Measured iterations per cell and backend (spec minimum 30).
    #[arg(long, alias = "n", default_value_t = 30)]
    runs: usize,
    /// Discarded warmup iterations per cell and backend.
    #[arg(long, alias = "warmup", default_value_t = 5)]
    warmups: usize,
    /// Timed steady-state calls per sandbox for each call.* metric.
    #[arg(long, default_value_t = 30)]
    calls: usize,
    /// Discarded warm calls per sandbox before call.* timing.
    #[arg(long, default_value_t = 5)]
    call_warmups: usize,
    /// Iterations for the resources cells (default: --runs).
    #[arg(long)]
    resource_runs: Option<usize>,
    /// Idle CPU window per sandbox, seconds (cpu.idle_60s).
    #[arg(long, default_value_t = 60)]
    idle_window_s: u64,
    /// Settle time before idle / post-workload memory samples, seconds.
    #[arg(long, default_value_t = 5)]
    settle_s: u64,
    /// Density memory budget: `75%` of MemTotal minus used at start, or bytes (`2GiB`).
    #[arg(long, default_value = "75%")]
    density_budget: String,
    /// Hard cap on sandboxes per density rep (spec: 500; default kept low for small hosts).
    #[arg(long, default_value_t = 16)]
    density_cap: usize,
    /// Density repetitions per backend and variant.
    #[arg(long, default_value_t = 3)]
    density_reps: usize,
    /// Stop density early when host MemAvailable drops below this many MiB.
    #[arg(long, default_value_t = 1024)]
    density_floor_mib: u64,
    /// MiB written + read per sandbox for post-workload / touched density.
    #[arg(long, default_value_t = 64)]
    touch_mib: u64,
    /// Smoke mode: tiny N everywhere, short windows; marks the result invalid.
    #[arg(long)]
    quick: bool,
    /// Accept N < 30 (the result is marked invalid).
    #[arg(long)]
    allow_small_n: bool,
    /// Only run cells whose key (`template/cache/workspace/scenario`) contains this.
    #[arg(long)]
    only: Option<String>,
    /// Seed for fixtures, cell order and bootstrap (default: random, recorded).
    #[arg(long)]
    seed: Option<u64>,
    /// Output directory for `<UTC>-<host>.json`.
    #[arg(long, default_value = "docs/bench/results")]
    out: PathBuf,
    /// Host label for the file name (default: hostname).
    #[arg(long)]
    host_label: Option<String>,
    /// Override the detected host kind (bare-metal | vm | vm-nested).
    #[arg(long)]
    host_kind: Option<String>,
    /// Operator declaration of other workloads on the host (noise.declared).
    #[arg(long, default_value = "")]
    noise_declared: String,
    /// 1-minute load above which the run is marked invalid (spec: 0.5).
    #[arg(long, default_value_t = 0.5)]
    max_load: f64,
    /// Files in the W2 read-only tree.
    #[arg(long, default_value_t = 5000)]
    w2_files: usize,
    /// Size of the W2 tree in MB (10^6 bytes).
    #[arg(long, default_value_t = 50)]
    w2_mb: u64,
    /// Scratch dir for state + fixtures (default: a temp dir).
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// Template directory.
    #[arg(long, default_value = "templates")]
    templates_dir: PathBuf,
    /// Static crabd (default: $SANDCRAB_CRABD, else
    /// <target dir>/<arch>-unknown-linux-musl/release/crabd, target dir $CARGO_TARGET_DIR or target/).
    #[arg(long, env = "SANDCRAB_CRABD")]
    crabd: Option<PathBuf>,
}

fn parse_list<T>(raw: &[String], f: impl Fn(&str) -> Result<T, String>) -> anyhow::Result<Vec<T>> {
    raw.iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| f(s).map_err(anyhow::Error::msg))
        .collect()
}

fn plan(a: RunArgs, work_dir: PathBuf) -> anyhow::Result<Plan> {
    let mut scenarios = BTreeSet::new();
    for s in &a.scenarios {
        scenarios.extend(Scenario::parse(s).map_err(anyhow::Error::msg)?);
    }
    let mut backends = parse_list(&a.backends, BackendId::parse)?;
    backends.dedup();
    let (runs, warmups, calls, call_warmups, density_cap, density_reps, idle, settle, touch) =
        if a.quick {
            (
                3,
                1,
                3,
                1,
                a.density_cap.min(3),
                1,
                2,
                1,
                a.touch_mib.min(8),
            )
        } else {
            (
                a.runs,
                a.warmups,
                a.calls,
                a.call_warmups,
                a.density_cap,
                a.density_reps,
                a.idle_window_s,
                a.settle_s,
                a.touch_mib,
            )
        };
    let resource_runs = if a.quick {
        1
    } else {
        a.resource_runs.unwrap_or(runs)
    };
    let allow_small_n = a.allow_small_n || a.quick;
    if (runs < run::MIN_N || resource_runs < run::MIN_N) && !allow_small_n {
        bail!(
            "--runs/--resource-runs below the spec minimum of {} need --allow-small-n \
             (the result is then marked invalid)",
            run::MIN_N
        );
    }
    let repo_root = std::env::current_dir()?;
    Ok(Plan {
        argv: std::env::args().collect(),
        backends,
        templates: a.templates.clone(),
        scenarios,
        workspaces: parse_list(&a.workspaces, Ws::parse)?,
        caches: parse_list(&a.caches, Cache::parse)?,
        runs,
        warmups,
        calls,
        call_warmups,
        resource_runs,
        idle_window: Duration::from_secs(idle),
        settle: Duration::from_secs(settle),
        density_budget: Budget::parse(&a.density_budget).map_err(anyhow::Error::msg)?,
        density_budget_raw: a.density_budget.clone(),
        density_cap,
        density_reps,
        density_floor: a.density_floor_mib << 20,
        touch_mib: touch,
        quick: a.quick,
        allow_small_n,
        only: a.only,
        seed: a.seed.unwrap_or_else(|| {
            u64::from(std::process::id()) ^ chrono::Utc::now().timestamp_micros() as u64
        }),
        out_dir: a.out,
        host_label: a.host_label,
        kind_override: a.host_kind,
        noise_declared: a.noise_declared,
        max_load: a.max_load,
        w2_files: a.w2_files,
        w2_bytes: a.w2_mb * 1_000_000,
        work_dir,
        templates_dir: a.templates_dir,
        crabd: a
            .crabd
            .unwrap_or_else(|| backend::default_crabd(&repo_root)),
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Command::Run(args) => {
            let work_dir = match &args.work_dir {
                Some(d) => d.clone(),
                None => tempfile_dir()?,
            };
            let plan = plan(*args, work_dir.clone())?;
            let res = tokio::select! {
                r = run::run(&plan) => r,
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("interrupted: sandboxes are destroyed on drop; run `sandcrab doctor --fix` to reap leftovers");
                    let _ = std::fs::remove_dir_all(&work_dir);
                    std::process::exit(130);
                }
            };
            if plan.work_dir.starts_with(std::env::temp_dir()) {
                let _ = std::fs::remove_dir_all(&plan.work_dir);
            }
            let (result, path) = res?;
            println!("{}", summarize::render(&result));
            println!("results: {}", path.display());
        }
        Command::HostInfo {
            host_kind,
            crabd,
            json,
        } => {
            let cwd = std::env::current_dir()?;
            let crabd = crabd.unwrap_or_else(|| backend::default_crabd(&cwd));
            let opts = OpenOpts {
                state_dir: sandcrab_sdk::core::default_state_root()?,
                templates_dir: cwd.join("templates"),
                crabd: crabd.clone(),
            };
            let h = host::collect(host_kind.as_deref(), &crabd, &backend::fc_paths(&opts));
            let n = host::noise_start("", 0.5);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "host": h, "noise": n }))?
                );
            } else {
                print!("{}", host::render(&h, &n));
            }
        }
        Command::Summarize { file } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?;
            let r: results::RunResult = serde_json::from_str(&text)
                .with_context(|| format!("{} is not a schema v1 results file", file.display()))?;
            print!("{}", summarize::render(&r));
        }
        Command::Schema => print!("{}", results::schema_json()),
    }
    Ok(())
}

/// A fresh scratch dir under the system temp dir (removed after the run).
fn tempfile_dir() -> anyhow::Result<PathBuf> {
    let d = std::env::temp_dir().join(format!(
        "scbench-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}
